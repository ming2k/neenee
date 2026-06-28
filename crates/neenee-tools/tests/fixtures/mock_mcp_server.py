#!/usr/bin/env python3
"""A tiny, dependency-free MCP stdio server used to validate neenee's MCP client.

Speaks the line-delimited JSON-RPC subset neenee uses:
  - initialize / notifications/initialized
  - tools/list
  - tools/call

Exposes two trivial tools: `echo` and `add`. Stderr is unused (neenee discards
it). Run standalone to experiment:

    echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | python3 mock_mcp_server.py
"""

import json
import sys

TOOLS = [
    {
        "name": "echo",
        "description": "Echo back the provided text.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "add",
        "description": "Add two integers and return the sum.",
        "inputSchema": {
            "type": "object",
            "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
            "required": ["a", "b"],
        },
    },
]


def text_result(text):
    return {"content": [{"type": "text", "text": text}]}


def handle(request):
    method = request.get("method")
    params = request.get("params") or {}

    if method == "initialize":
        return {
            "protocolVersion": params.get("protocolVersion", "2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-mcp", "version": "0.1.0"},
        }
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "echo":
            return text_result(str(args.get("text", "")))
        if name == "add":
            return text_result(str(int(args.get("a", 0)) + int(args.get("b", 0))))
        raise ValueError(f"unknown tool: {name}")
    raise ValueError(f"unknown method: {method}")


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        request = json.loads(line)
        # Notifications (no id) get no response.
        if "id" not in request:
            continue
        response = {"jsonrpc": "2.0", "id": request["id"]}
        try:
            response["result"] = handle(request)
        except Exception as error:  # noqa: BLE001 - report any failure as JSON-RPC error
            response["error"] = {"code": -32000, "message": str(error)}
        sys.stdout.write(json.dumps(response) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
