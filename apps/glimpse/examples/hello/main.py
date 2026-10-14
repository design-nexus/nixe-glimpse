import json, sys, uuid, datetime
for line in sys.stdin:
    request = json.loads(line)
    op = request["op"]
    result = None
    if op == "hello":
        reply = {"id": request["id"], "api_version": 1, "capabilities": ["search"]}
    else:
        if op == "search":
            query = request["args"]["query"].strip().lower()
            result = []
            if query == "uuid":
                text = str(uuid.uuid4())
                result.append({"id": "uuid", "title": text, "subtitle": "New UUID · copy", "action": {"kind": "copy", "text": text}})
            elif query == "utc":
                text = datetime.datetime.now(datetime.timezone.utc).isoformat()
                result.append({"id": "utc", "title": text, "subtitle": "UTC time · copy", "action": {"kind": "copy", "text": text}})
        reply = {"id": request["id"], "result": result}
    print(json.dumps(reply), flush=True)
    if op == "shutdown":
        break
