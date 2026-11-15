"""Worker lifetime rules shared by async storage APIs. No background write is abandoned."""
import asyncio


async def _join(task):
    interrupted = None
    while not task.done():
        try: await asyncio.shield(task)
        except asyncio.CancelledError as error: interrupted = error
        except Exception: break
    try: value = task.result()
    except Exception:
        if interrupted is not None: raise interrupted
        raise
    return value, interrupted


async def storage_call(callback, *args, discard=None, on_result=None):
    value, interrupted = await _join(asyncio.create_task(asyncio.to_thread(callback, *args)))
    if on_result is not None: on_result(value)
    if interrupted is not None:
        if discard is not None:
            # A resource produced after cancellation must be closed before returning.
            await _join(asyncio.create_task(asyncio.to_thread(discard, value)))
        raise interrupted
    return value
