"""A multi-framework agent app exercising every agent-pack import `keel
doctor` recognizes: google-adk, google-genai, pydantic-ai, openai-agents,
crewai, langgraph, and mcp."""

import crewai
import google.adk
import mcp
from agents import Agent as OpenAIAgent
from google.genai import types
from langgraph.graph import StateGraph
from pydantic_ai import Agent


def build():
    return google.adk, crewai, mcp, OpenAIAgent, types, StateGraph, Agent
