"""rustsolverr Camoufox sidecar.

Started by the Rust process. Reads one JSON command per line on stdin and
writes one JSON reply per line on stdout. Logs go to stderr. Exits, and
closes the browser, when stdin closes.

Commands:
  {"op": "solve", "id": N, "url": ..., "postScript": str|null, "cookies": [...],
   "proxy": {...}|null, "timeoutMs": N, "waitMs": N, "classifier": js}
  {"op": "cancel", "id": N}
  {"op": "shutdown"}
Replies:
  {"event": "ready"}
  {"id": N, "ok": true, "page": {url, status, headers, body, cookies, userAgent}}
  {"id": N, "ok": false, "kind": "timeout"|"blocked"|"error", "error": str}
"""

import asyncio
import json
import os
import sys

from camoufox.async_api import AsyncCamoufox


class Blocked(Exception):
    pass


def emit(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def log(*parts):
    print("[sidecar]", *parts, file=sys.stderr, flush=True)


async def click_turnstile(page):
    """Best effort: click the checkbox of a Cloudflare Turnstile widget."""
    for frame in page.frames:
        if "challenges.cloudflare.com" not in frame.url:
            continue
        try:
            element = await frame.frame_element()
            box = await element.bounding_box()
            if box:
                await page.mouse.click(box["x"] + 28, box["y"] + box["height"] / 2)
        except Exception as error:  # the frame can go away at any time
            log("turnstile click failed:", error)


async def solve(browser, msg):
    loop = asyncio.get_running_loop()
    timeout_ms = msg["timeoutMs"]
    deadline = loop.time() + timeout_ms / 1000
    options = {"proxy": msg["proxy"]} if msg.get("proxy") else {}
    context = await browser.new_context(**options)
    try:
        if msg["cookies"]:
            await context.add_cookies(msg["cookies"])
        page = await context.new_page()
        document = {}

        def on_response(response):
            if response.frame == page.main_frame and response.request.is_navigation_request():
                document["response"] = response

        page.on("response", on_response)

        if msg.get("postScript"):
            async with page.expect_navigation(wait_until="domcontentloaded", timeout=timeout_ms):
                await page.evaluate(msg["postScript"])
        else:
            await page.goto(msg["url"], wait_until="domcontentloaded", timeout=timeout_ms)

        ticks = 0
        while True:
            try:
                state = await page.evaluate(msg["classifier"])
            except Exception:  # the page is navigating
                state = "challenge"
            if state == "denied":
                raise Blocked()
            if state == "clear":
                break
            if loop.time() >= deadline:
                raise asyncio.TimeoutError()
            ticks += 1
            if ticks >= 8 and (ticks - 8) % 10 == 0:
                await click_turnstile(page)
            await asyncio.sleep(0.5)

        if ticks:
            try:
                await page.wait_for_load_state("domcontentloaded", timeout=5000)
            except Exception:
                pass
        if msg.get("waitMs"):
            await asyncio.sleep(msg["waitMs"] / 1000)

        response = document.get("response")
        return {
            "url": page.url,
            "status": response.status if response else 200,
            "headers": await response.all_headers() if response else {},
            "body": await page.content(),
            "cookies": await context.cookies(),
            "userAgent": await page.evaluate("navigator.userAgent"),
        }
    finally:
        await context.close()


async def run(browser, msg, tasks):
    request_id = msg["id"]
    try:
        page = await asyncio.wait_for(solve(browser, msg), msg["timeoutMs"] / 1000)
        emit({"id": request_id, "ok": True, "page": page})
    except asyncio.TimeoutError:
        emit({"id": request_id, "ok": False, "kind": "timeout", "error": "timeout"})
    except Blocked:
        emit({"id": request_id, "ok": False, "kind": "blocked", "error": "blocked"})
    except asyncio.CancelledError:
        raise  # cancelled by the Rust side: it does not wait for a reply
    except Exception as error:
        kind = "timeout" if type(error).__name__ == "TimeoutError" else "error"
        emit({"id": request_id, "ok": False, "kind": kind, "error": str(error)})
    finally:
        tasks.pop(request_id, None)


async def serve():
    headless = os.environ.get("HEADLESS", "true").lower()
    headless = "virtual" if headless == "virtual" else headless != "false"
    loop = asyncio.get_running_loop()
    async with AsyncCamoufox(
        headless=headless,
        disable_coop=True,
        i_know_what_im_doing=True,
    ) as browser:
        emit({"event": "ready"})
        tasks = {}
        while True:
            line = await loop.run_in_executor(None, sys.stdin.readline)
            if not line:
                break
            msg = json.loads(line)
            if msg["op"] == "solve":
                tasks[msg["id"]] = asyncio.create_task(run(browser, msg, tasks))
            elif msg["op"] == "cancel":
                task = tasks.get(msg["id"])
                if task:
                    task.cancel()
            elif msg["op"] == "shutdown":
                break
        pending = list(tasks.values())
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)


async def main():
    try:
        await serve()
    except Exception as error:
        log("fatal:", repr(error))
    finally:
        # The browser is closed here. Exit hard: asyncio.run would wait for
        # the thread that is blocked reading stdin.
        sys.stdout.flush()
        os._exit(0)


if __name__ == "__main__":
    asyncio.run(main())
