# Thin self-hosted mem0 server for the benchmark (mem0 lib + FastAPI).
FROM python:3.12-slim
WORKDIR /app
RUN pip install --no-cache-dir \
    "mem0ai" \
    "fastapi" \
    "uvicorn[standard]" \
    "qdrant-client" \
    "openai"
COPY server.py /app/server.py
EXPOSE 8000
CMD ["uvicorn", "server:app", "--host", "0.0.0.0", "--port", "8000"]
