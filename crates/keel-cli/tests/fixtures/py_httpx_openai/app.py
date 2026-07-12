"""A tiny agent that calls an HTTP API and an LLM."""

import httpx
from openai import OpenAI

DATA_API = "https://api.example.com/v1/data"


def fetch_data():
    return httpx.get(DATA_API).json()


def ask(client: OpenAI, prompt: str) -> str:
    resp = client.responses.create(model="gpt-4.1", input=prompt)
    return resp.output_text
