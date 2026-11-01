"""Stands in for `devin acp` in tests: it speaks the Agent Client
Protocol (JSON-RPC 2.0, one message per line) over stdio the way Devin
does, and plays the scenario `--scenario` names, a JSON file:

- `log`: where every line the client sends is appended.
- `session_id`: what session/new answers with.
- `model`, `models`: the model config option's current value and its
  options, as `{"value", "name"}`.
- `load`: the updates session/load replays before it answers.
- `gate`: session/new waits until this file exists, for up to 10 s.
- `plan_file`: `{"path", "text"}`, written at the start.
- `prompts`: what a session/prompt does: the first entry whose `mode`
  (if any) is the mode set last and whose `match` (if any) is in the
  prompt's JSON. Its `steps` run in order:
  - `{"update": U}` sends a session/update with U.
  - `{"request": METHOD, "params": P}` sends a request and waits for the
    client's reply; with `"wait": false`, the steps go on without it and
    the reply is only logged.
  - `{"end": REASON}` answers the prompt with that stop reason.
  - `{"stderr": TEXT}` writes TEXT to stderr.
  - `{"exit": true}` exits without answering.

Steps that run out without an `end` leave the prompt running.

session/cancel answers the running prompt with `cancelled`. In every
string of a step, `{turn}` becomes an ID unique to the prompt.
"""

import argparse
import json
import os
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument("--scenario", required=True)
parser.add_argument("command", nargs="?")
args = parser.parse_args()
with open(args.scenario) as f:
    scenario = json.load(f)


def log(line):
    with open(scenario["log"], "a") as f:
        f.write(line)


if "plan_file" in scenario:
    with open(scenario["plan_file"]["path"], "w") as f:
        f.write(scenario["plan_file"]["text"])


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def answer(id, result):
    send({"jsonrpc": "2.0", "id": id, "result": result})


def update(u):
    params = {"sessionId": scenario.get("session_id", ""), "update": u}
    send({"jsonrpc": "2.0", "method": "session/update", "params": params})


def config_options(listed):
    model = {"id": "model", "currentValue": scenario.get("model", "swe-2")}
    if listed:
        model["options"] = scenario.get("models", [])
    return [model]


def substitute(value, turn):
    if isinstance(value, str):
        return value.replace("{turn}", turn)
    if isinstance(value, list):
        return [substitute(v, turn) for v in value]
    if isinstance(value, dict):
        return {k: substitute(v, turn) for k, v in value.items()}
    return value


mode = None
prompts = 0
requests = 0
# The running prompt's id, the steps it has left, and the id of the
# request whose reply they wait for.
prompt = None
steps = []
waiting = None


def run_steps():
    global prompt, steps, waiting, requests
    while prompt is not None and steps and waiting is None:
        step = steps.pop(0)
        if "update" in step:
            update(step["update"])
        elif "request" in step:
            requests += 1
            id = f"{step['request']}-{requests}"
            request = {"jsonrpc": "2.0", "id": id, "method": step["request"]}
            send(dict(request, params=step.get("params", {})))
            if step.get("wait", True):
                waiting = id
        elif "end" in step:
            answer(prompt, {"stopReason": step["end"]})
            prompt, steps = None, []
        elif "stderr" in step:
            sys.stderr.write(step["stderr"] + "\n")
            sys.stderr.flush()
        elif step.get("exit"):
            sys.exit(0)


def pick(params):
    text = json.dumps(params.get("prompt", []))
    for p in scenario.get("prompts", []):
        if p.get("mode", mode) == mode and p.get("match", "") in text:
            return p["steps"]
    return [{"end": "end_turn"}]


for line in sys.stdin:
    log(line)
    m = json.loads(line)
    id = m.get("id")
    method = m.get("method")
    params = m.get("params", {})
    if method is None:
        if id is not None and id == waiting:
            waiting = None
            run_steps()
    elif method == "initialize":
        answer(id, {"protocolVersion": 1})
    elif method == "session/new":
        # A gate that never opens ends the fake rather than leave it
        # behind.
        deadline = time.monotonic() + 10
        while "gate" in scenario and not os.path.exists(scenario["gate"]):
            if time.monotonic() > deadline:
                sys.exit(1)
            time.sleep(0.01)
        answer(
            id,
            {
                "sessionId": scenario.get("session_id", "fake-session"),
                "configOptions": config_options(True),
            },
        )
    elif method == "session/load":
        for u in scenario.get("load", []):
            update(u)
        answer(id, {"configOptions": config_options(False)})
    elif method == "session/set_mode":
        mode = params.get("modeId")
        answer(id, {})
    elif method == "session/set_config_option":
        answer(id, {"configOptions": config_options(False)})
    elif method == "session/delete":
        answer(id, {})
    elif method == "session/prompt":
        prompts += 1
        prompt = id
        steps = substitute(pick(params), f"{os.getpid()}-{prompts}")
        run_steps()
    elif method == "session/cancel":
        if prompt is not None:
            answer(prompt, {"stopReason": "cancelled"})
        prompt, steps, waiting = None, [], None
    elif id is not None:
        send(
            {
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "unknown method"},
            }
        )
