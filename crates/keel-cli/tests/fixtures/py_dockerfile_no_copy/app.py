import httpx

def render(op: str) -> dict:
    return httpx.post("https://us-central1-aiplatform.googleapis.com/v1/x:fetchPredictOperation", json={"operationName": op}).json()
