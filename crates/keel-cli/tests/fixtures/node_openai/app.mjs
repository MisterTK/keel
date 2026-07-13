// A tiny agent script that calls OpenAI through the official SDK.
import OpenAI from "openai";

const client = new OpenAI();

export async function complete(prompt) {
  return client.responses.create({ model: "gpt-5", input: prompt });
}
