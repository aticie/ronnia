FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim

WORKDIR /app

ENV PYTHONPATH=/app
ENV UV_COMPILE_BYTECODE=1
ENV UV_LINK_MODE=copy

COPY . /app

RUN uv sync --locked --no-dev

ENTRYPOINT ["uv", "run", "python", "ronnia/main.py"]