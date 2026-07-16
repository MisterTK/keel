"""A real ``google-adk`` ``LlmAgent`` with one ``FunctionTool``, proving Keel's
Tier 1 resilience applies to an agent's tool calls with zero agent-code
changes — the same story as ``agent-demo``, but through a real ADK agent loop
instead of a bare script.

``fetch_answer()`` is an ordinary ``httpx.get`` against a fake, flaky
completion endpoint (``tools/faultproxy``, reusing ``agent-demo``'s
429/429/200 scenario). Keel's retry lives BELOW the agent loop, inside that
one tool call: 3 upstream calls happen, but the agent itself takes exactly
ONE turn to invoke the tool and ONE turn to answer — the 429 storm never
reaches the LLM, so zero extra tokens are burned riding it out.

No live LLM: a scripted ``BaseLlm`` drives exactly those two turns (a
function-call turn, then a final-text turn quoting the tool's real, retried
result) so the demo is deterministic and needs no credentials — same
approach as the farm's ``ScriptedModel`` (see
``python/keel/tests/test_farm_adk_composition.py``), adapted here to run
standalone against a scripted fake instead of a live google-adk farm venv.
"""

from __future__ import annotations

import asyncio
import os
import sys
from typing import Any

try:
    from google.adk.agents.llm_agent import LlmAgent
    from google.adk.models.base_llm import BaseLlm
    from google.adk.models.llm_response import LlmResponse
    from google.adk.runners import InMemoryRunner
    from google.adk.tools.function_tool import FunctionTool
    from google.genai import types
except ImportError:
    print("adk-demo needs google-adk (pip install google-adk) — skipping.")
    sys.exit(0)

import httpx


def fetch_answer() -> dict[str, str]:
    """The agent's one tool: a plain, intercepted HTTP GET. Keel's retry
    policy applies here with zero code changes — the exact seam
    ``agent-demo``'s bare ``httpx.get`` uses, just called from inside a real
    ADK tool body instead of top-level script code."""
    url = os.environ["KEEL_DEMO_URL"]
    resp = httpx.get(url, timeout=10.0)
    resp.raise_for_status()
    return resp.json()


class ScriptedModel(BaseLlm):
    """A ``BaseLlm`` that never calls out to a real model: turn 1 invokes the
    ``fetch_answer`` tool, turn 2 quotes its real (already-retried) result as
    the final answer. Exactly one agent turn is spent on the tool call — the
    429/429/200 storm underneath it is invisible to the LLM."""

    model: str = "scripted-adk-demo-model"
    turn: int = 0

    async def generate_content_async(self, llm_request: Any, stream: bool = False):
        self.turn += 1
        if self.turn == 1:
            part = types.Part(function_call=types.FunctionCall(name="fetch_answer", args={}))
        else:
            # Quote the REAL tool result out of the request history rather
            # than hard-coding it, so this only prints "reply=42" because the
            # retried httpx.get actually returned it.
            reply = None
            for content in llm_request.contents:
                for p in content.parts or []:
                    fr = getattr(p, "function_response", None)
                    if fr is not None and fr.name == "fetch_answer":
                        reply = fr.response.get("reply")
            part = types.Part(text=f"reply={reply}")
        yield LlmResponse(content=types.Content(role="model", parts=[part]), partial=False)


async def _run_agent() -> str | None:
    model = ScriptedModel()
    agent = LlmAgent(name="adk_demo_agent", model=model, tools=[FunctionTool(func=fetch_answer)])
    runner = InMemoryRunner(agent=agent, app_name="adk-demo")
    session = await runner.session_service.create_session(app_name="adk-demo", user_id="u1")
    final_text = None
    async for event in runner.run_async(
        user_id="u1",
        session_id=session.id,
        new_message=types.Content(role="user", parts=[types.Part(text="go")]),
    ):
        if event.content and event.content.parts:
            for p in event.content.parts:
                if p.text:
                    final_text = p.text
    assert model.turn == 2, f"expected exactly one tool turn + one final turn, got {model.turn} turns"
    return final_text


if __name__ == "__main__":
    reply_text = asyncio.run(asyncio.wait_for(_run_agent(), timeout=30))
    sys.stdout.write(f"{reply_text}\n")
