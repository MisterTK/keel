"""Fixture for `keel flows suggest`: one rich, replay-safe candidate with
idempotent-unsafe effects and virtualized reads, one replay-unsafe candidate,
and one pure helper that is not a candidate at all."""

import random
import subprocess
import time

import httpx
from openai import OpenAI

API = "https://api.example.com/v1/data"
client = OpenAI()


def ingest():
    started = time.time()
    seed = random.random()
    data = httpx.get(API).json()
    httpx.post(API, json=data)
    client.responses.create(model="gpt-4.1", input="hi")
    return started, seed, data


def export_report():
    httpx.get(API)
    subprocess.run(["ls"])


def helper():
    return 41 + 1
