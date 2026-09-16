import time
from google import genai


def wait(client, op):
    while not client.operations.get(op).done:
        time.sleep(1)
    return op


def wait_guarded(client, op):
    while True:
        if client.operations.get(op).done:
            return op
        time.sleep(1)
