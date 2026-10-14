import json, pathlib, sys, os
storage = pathlib.Path(os.environ.get("GLIMPSE_STORAGE", ".")) / "preferences.json"
for line in sys.stdin:
    request = json.loads(line)
    op = request["op"]
    try:
        if op == "hello":
            reply = {"id": request["id"], "api_version": 1, "capabilities": ["reader", "settings"]}
        elif op == "shutdown":
            break
        else:
            if op == "extract":
                with open(request["args"]["path"], "r", encoding="utf-8") as source:
                    text = source.read(65536)
                result = [{"text": text, "line": 1}]
            elif op == "settings":
                preferences = json.loads(storage.read_text()) if storage.exists() else {}
                result = {"rows": [{"kind": "switch", "key": "show_label", "title": "Show label", "value": preferences.get("show_label", True)}]}
            elif op == "configure":
                if request["args"]["key"] != "show_label" or not isinstance(request["args"]["value"], bool):
                    raise ValueError("Invalid preference")
                storage.write_text(json.dumps({"show_label": request["args"]["value"]}))
                result = None
            else:
                raise ValueError("Unsupported operation")
            reply = {"id": request["id"], "result": result}
    except Exception as error:
        reply = {"id": request["id"], "error": str(error)}
    print(json.dumps(reply), flush=True)
