FROM python:3.12-slim
WORKDIR /code
COPY pyproject.toml uv.lock ./
COPY agents/ agents/
CMD ["python", "app.py"]
