FROM python:3.12-slim

# Install git (needed for thin-diff prepare step)
RUN apt-get update && apt-get install -y --no-install-recommends git && rm -rf /var/lib/apt/lists/*

# Install uv
RUN pip install uv

WORKDIR /app
