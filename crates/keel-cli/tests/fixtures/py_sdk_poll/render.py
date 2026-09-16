import time
from google import genai
from .clients import get_client


def poll_video_takes(op, model_id, timeout_s=900):
    client = get_client()
    deadline = time.monotonic() + timeout_s
    while not op.done:
        if time.monotonic() > deadline:
            raise TimeoutError(f"Video render timed out ({model_id})")
        time.sleep(10)
        op = client.operations.get(op)
    return op.result
