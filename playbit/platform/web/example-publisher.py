"""Export only the public publisher evidence for the bundled browser examples."""
import json
from pathlib import Path
import subprocess
import sys


def collect(mcp, directory):
    examples = ("todo", "micro-kart", "shared-note")
    requests = [
        dict(jsonrpc="2.0", id=name, method="tools/call", params=dict(
            name="playbit", arguments=dict(op="authenticate-package", bundle=str(directory / (name + ".pbapp")))))
        for name in examples
    ]
    process = subprocess.run([mcp], input="".join(json.dumps(item) + "\n" for item in requests),
                             text=True, stdout=subprocess.PIPE, check=True, timeout=120)
    publisher = None
    evidence = None
    for name, line in zip(examples, process.stdout.splitlines(), strict=True):
        reply = json.loads(line)
        if reply.get("id") != name or "result" not in reply:
            raise ValueError(f"Unexpected authentication response for {name}")
        result = reply["result"]
        message = result["content"][0]["text"]
        if result.get("isError"):
            raise ValueError(f"Could not authenticate {name}: {message}")
        identity = json.loads(message)
        app = identity["app"]
        current = {key: identity[key] for key in ("issuer", "status")}
        if app["app_id"] != name or not app["publisher"] or any(
            not isinstance(current[key], list) or len(current[key]) != size
            or any(type(byte) is not int or not 0 <= byte <= 255 for byte in current[key])
            for key, size in (("issuer", 32), ("status", 162))
        ):
            raise ValueError(f"Invalid authenticated publisher evidence for {name}")
        if evidence is not None and (current != evidence or app["publisher"] != publisher):
            raise ValueError("The bundled examples must use the same authenticated publisher key")
        evidence = current
        publisher = app["publisher"]
    return evidence


if __name__ == "__main__":
    output = Path(sys.argv[2]).resolve(strict=True)
    try:
        evidence = collect(sys.argv[1], output)
    except ValueError as error:
        sys.exit(f"Browser examples: {error}")
    with (output / "example-publisher.json").open("x") as stream:
        json.dump(evidence, stream, separators=(",", ":"))
        stream.write("\n")
