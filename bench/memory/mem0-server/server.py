"""Minimal self-hosted mem0 server for the benchmark.

A thin FastAPI shim over the `mem0` library, exposing exactly the OSS REST
surface the Baybo mem0 backend speaks in `self_hosted` mode (`/memories`,
`/search`). Extraction runs synchronously inside `add` (an LLM call), so there
is no event feed — the backend's settle is a no-op.

LLM + embedder are pulled from the SAME env vars OpenViking uses, so the two
arms are aligned by construction:
  - LLM      : OPENVIKING_VLM_MODEL @ OPENVIKING_VLM_API_BASE, key DEEPSEEK_API_KEY
  - embedder : OPENVIKING_EMBEDDING_MODEL @ OPENVIKING_EMBEDDING_URL
Vector store is qdrant (dims = MEM0_EMBED_DIMS, default 2560 for Qwen3-4B).
"""

import os

from fastapi import FastAPI, Request
from mem0 import Memory

# Embedding output width. Single source shared with ov.conf via .env
# (OPENVIKING_EMBEDDING_DIMS); MEM0_EMBED_DIMS kept as a fallback alias.
EMBED_DIMS = int(
    os.environ.get("OPENVIKING_EMBEDDING_DIMS")
    or os.environ.get("MEM0_EMBED_DIMS")
    or "2560"
)


def _llm_base() -> str:
    # mem0's openai client wants the full base incl. /v1; OpenViking's vlm
    # api_base omits it.
    base = os.environ.get("OPENVIKING_VLM_API_BASE", "https://api.deepseek.com").rstrip("/")
    return base if base.endswith("/v1") else base + "/v1"


CONFIG = {
    "llm": {
        "provider": "openai",
        "config": {
            "model": os.environ.get("OPENVIKING_VLM_MODEL", "deepseek-chat"),
            "openai_base_url": _llm_base(),
            "api_key": os.environ.get("DEEPSEEK_API_KEY", ""),
        },
    },
    "embedder": {
        "provider": "openai",
        "config": {
            "model": os.environ.get("OPENVIKING_EMBEDDING_MODEL", ""),
            "openai_base_url": os.environ.get("OPENVIKING_EMBEDDING_URL", ""),
            "api_key": os.environ.get("OPENVIKING_EMBEDDING_API_KEY") or "x",
            # No `embedding_dims` here — that makes mem0 send a `dimensions`
            # param the embeddings API, which Qwen3 (non-matryoshka) rejects.
            # The model's native width is fixed; the vector store is sized below.
        },
    },
    "vector_store": {
        "provider": "qdrant",
        "config": {
            "host": os.environ.get("QDRANT_HOST", "qdrant"),
            "port": int(os.environ.get("QDRANT_PORT", "6333")),
            "collection_name": os.environ.get("MEM0_COLLECTION", "mem0_bench"),
            "embedding_model_dims": EMBED_DIMS,
        },
    },
}

mem = Memory.from_config(CONFIG)
app = FastAPI(title="mem0-bench")


def _wrap(res):
    """mem0 returns {"results": [...]} on recent versions, a bare list on older
    ones. The Baybo backend accepts either; normalize to the dict shape."""
    return res if isinstance(res, dict) else {"results": res}


def _scope(body: dict) -> dict:
    return {k: body[k] for k in ("user_id", "agent_id", "run_id", "metadata") if body.get(k) is not None}


@app.get("/health")
def health():
    return {"status": "ok"}


@app.post("/memories")
async def add(req: Request):
    body = await req.json()
    messages = body.get("messages") or body.get("text")
    kwargs = _scope(body)
    if "infer" in body:
        kwargs["infer"] = body["infer"]
    return _wrap(mem.add(messages, **kwargs))


@app.post("/search")
async def search(req: Request):
    body = await req.json()
    # Newer mem0 requires entity scope via `filters`, not top-level kwargs.
    filters = {k: body[k] for k in ("user_id", "agent_id", "run_id") if body.get(k) is not None}
    return _wrap(mem.search(body.get("query", ""), limit=int(body.get("limit", 10)), filters=filters or None))


@app.get("/memories")
def list_all(user_id: str | None = None, agent_id: str | None = None):
    filters = {}
    if user_id:
        filters["user_id"] = user_id
    if agent_id:
        filters["agent_id"] = agent_id
    return _wrap(mem.get_all(filters=filters or None))


@app.get("/memories/{memory_id}")
def get(memory_id: str):
    return mem.get(memory_id) or {}


@app.put("/memories/{memory_id}")
async def update(memory_id: str, req: Request):
    body = await req.json()
    return _wrap(mem.update(memory_id, data=body.get("text", "")))


@app.delete("/memories/{memory_id}")
def delete(memory_id: str):
    mem.delete(memory_id)
    return {"message": f"Memory {memory_id} deleted."}


@app.delete("/memories")
def delete_all(user_id: str | None = None):
    mem.delete_all(**({"user_id": user_id} if user_id else {}))
    return {"message": "All memories deleted."}
