#!/usr/bin/env python3
"""JSON-RPC stdio helper for calling ferrosa-memory MCP tools from bash.

Usage:
    python3 mcp_helper.py <mcp_binary> <tool_name> [args_json]

Starts the MCP server, sends initialize handshake, calls the tool,
prints the result JSON to stdout, then exits.

Exit codes:
    0  — success (result on stdout)
    1  — tool returned an error
    2  — protocol/connection error
"""

import json
import subprocess
import sys
import os


def main():
    if len(sys.argv) < 3:
        print("Usage: mcp_helper.py <mcp_binary> <tool_name> [args_json]", file=sys.stderr)
        sys.exit(2)

    binary = sys.argv[1]
    tool_name = sys.argv[2]
    args_json = sys.argv[3] if len(sys.argv) > 3 else "{}"

    try:
        args = json.loads(args_json)
    except json.JSONDecodeError as e:
        print(f"Invalid args JSON: {e}", file=sys.stderr)
        sys.exit(2)

    env = os.environ.copy()
    env["RUST_LOG"] = "warn"
    # Ensure the config is found (macOS dirs::config_dir != ~/.config)
    if "FERROSA_MEMORY_CONFIG" not in env:
        xdg_config = os.path.expanduser("~/.config/ferrosa-memory.toml")
        if os.path.exists(xdg_config):
            env["FERROSA_MEMORY_CONFIG"] = xdg_config

    proc = subprocess.Popen(
        [binary],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        env=env,
    )

    def send(msg):
        line = json.dumps(msg) + "\n"
        proc.stdin.write(line.encode())
        proc.stdin.flush()

    def recv():
        line = proc.stdout.readline()
        if not line:
            return None
        return json.loads(line)

    def recv_for_id(request_id, max_messages=10):
        """Read messages until we get the response matching request_id."""
        for _ in range(max_messages):
            msg = recv()
            if msg is None:
                return None
            if msg.get("id") == request_id:
                return msg
        return None

    try:
        # Initialize handshake
        send({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "dikw-test", "version": "1.0"},
            },
        })
        resp = recv_for_id(1)
        if resp is None:
            print("MCP server closed without responding to initialize", file=sys.stderr)
            sys.exit(2)

        # Send initialized notification (server may send a null response)
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})

        # Call the tool
        send({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": tool_name, "arguments": args},
        })
        result = recv_for_id(2)
        if result is None:
            print("MCP server closed without responding to tool call", file=sys.stderr)
            sys.exit(2)

        if "error" in result:
            print(json.dumps(result["error"]), file=sys.stderr)
            sys.exit(1)

        # Extract the text content from MCP response
        res = result.get("result") or {}
        content = res.get("content", [])
        if content and isinstance(content, list):
            text = content[0].get("text", "")
            # Try to parse as JSON for structured output
            try:
                parsed = json.loads(text)
                print(json.dumps(parsed))
            except (json.JSONDecodeError, TypeError):
                print(text)
        else:
            print(json.dumps(res))

    finally:
        proc.stdin.close()
        proc.terminate()
        proc.wait(timeout=5)


if __name__ == "__main__":
    main()
