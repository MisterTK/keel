import httpx


def main() -> dict:
    return httpx.get("https://api.example.com/status").json()
