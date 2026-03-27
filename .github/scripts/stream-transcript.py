#!/usr/bin/env python3
"""Real-time formatter for Claude Code stream-json output.

Reads JSONL from stdin, prints human-readable transcript to stdout as it streams.
Designed to be piped: claude -p ... --output-format stream-json 2>&1 | python3 stream-transcript.py
"""

import json
import sys


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except Exception:
            # Non-JSON line (e.g. error message from claude)
            print(line, flush=True)
            continue

        t = msg.get("type", "")

        if t == "assistant":
            for block in msg.get("message", msg).get("content", []):
                if block.get("type") == "text":
                    print(f"\n💬 {block['text']}", flush=True)
                elif block.get("type") == "tool_use":
                    name = block["name"]
                    inp = block.get("input", {})
                    if name == "Bash":
                        print(f"\n$ {inp.get('command', '')}", flush=True)
                    elif name == "Read":
                        print(f"\n📄 Read {inp.get('file_path', '')}", flush=True)
                    elif name == "Edit":
                        path = inp.get("file_path", "")
                        old = inp.get("old_string", "")
                        new = inp.get("new_string", "")
                        print(f"\n✏️  Edit {path}", flush=True)
                        for l in old.splitlines():
                            print(f"  - {l}", flush=True)
                        for l in new.splitlines():
                            print(f"  + {l}", flush=True)
                    elif name == "Write":
                        print(f"\n📝 Write {inp.get('file_path', '')}", flush=True)
                    elif name == "Glob":
                        print(f"\n🔍 Glob {inp.get('pattern', '')}", flush=True)
                    elif name == "Grep":
                        print(f"\n🔍 Grep '{inp.get('pattern', '')}' in {inp.get('path', '.')}", flush=True)
                    else:
                        print(f"\n🔧 {name} {json.dumps(inp)[:300]}", flush=True)

        elif t == "user":
            for block in msg.get("message", msg).get("content", []):
                if block.get("type") == "tool_result":
                    content = block.get("content", "")
                    if isinstance(content, list):
                        content = "\n".join(
                            b.get("text", str(b)) for b in content
                        )
                    content = str(content)

                    # Compact file reads
                    fr = (msg.get("tool_use_result") or {}).get("file", {})
                    if fr:
                        path = fr.get("filePath", "")
                        lines = fr.get("totalLines", "?")
                        print(f"  → {path} ({lines} lines)", flush=True)
                    elif "(Bash completed with no output)" in content:
                        print("  → (ok)", flush=True)
                    elif len(content) > 1000:
                        print(f"  → {content[:1000]}\n  ... (truncated)", flush=True)
                    elif content:
                        for l in content.splitlines():
                            print(f"  {l}", flush=True)

        elif t == "result":
            # Final result
            text = msg.get("result", "")
            if text:
                print(f"\n{'='*60}", flush=True)
                print(f"✅ {text}", flush=True)


if __name__ == "__main__":
    main()
