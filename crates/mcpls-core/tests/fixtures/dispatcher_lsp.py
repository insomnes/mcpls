"""External LSP fixture. Its authored workspace marker controls hover and diagnostics."""
import json
import os
from pathlib import Path
import sys
import time

root = Path.cwd()
events = Path(sys.argv[1])


def event(kind, **fields):
    with events.open("a") as stream:
        stream.write(json.dumps({"event": kind, "pid": os.getpid(), "cwd": str(root), **fields}) + "\n")


def send(message):
    encoded = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(encoded)).encode() + b"\r\n\r\n" + encoded)
    sys.stdout.buffer.flush()


def wait_for_release(gate):
    while gate.exists():
        time.sleep(0.01)


event("start")
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode().split(":", 1)
        headers[key.lower()] = value.strip()
    request = json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
    method = request.get("method")
    params = request.get("params", {})
    event("request", method=method)
    if method == "exit":
        break
    if method == "textDocument/didOpen":
        uri = params["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": uri, "diagnostics": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                                        "message": "problem " + (root / "marker").read_text(), "severity": 1}]}})
    if "id" not in request:
        continue
    if method == "initialize":
        event("initialize", params=params)
        wait_for_release(root / ".hold-start")
        if (root / ".fail-start").exists():
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32603, "message": "authored initialize failure"}})
            continue
        result = {"capabilities": {"hoverProvider": True, "definitionProvider": True, "documentSymbolProvider": True,
                                   "workspaceSymbolProvider": True, "callHierarchyProvider": True}}
    elif method == "textDocument/hover":
        event("hover")
        wait_for_release(root / ".hold-response")
        result = {"contents": {"kind": "plaintext", "value": (root / "marker").read_text()}}
    elif method == "textDocument/documentSymbol":
        result = [{"name": "symbol" + str(index), "kind": 12,
                   "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                   "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}
                  for index in range(3)]
    elif method == "textDocument/prepareCallHierarchy":
        result = [{"name": "entry", "kind": 12, "uri": params["textDocument"]["uri"], "data": {"marker": (root / "marker").read_text()},
                   "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                   "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]
    elif method == "callHierarchy/incomingCalls":
        result = [{"from": params["item"], "fromRanges": [params["item"]["range"]]}]
    elif method == "callHierarchy/outgoingCalls":
        result = [{"to": params["item"], "fromRanges": [params["item"]["range"]]}]
    elif method == "workspace/symbol":
        result = []
    else:
        result = None
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})
