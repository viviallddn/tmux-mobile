#!/usr/bin/env bash
#
# usage.sh - best-effort token usage reporter for Team agents.
#
# Claude Code and Codex persist authoritative token counters locally, but in
# different JSONL formats. This hook normalizes the latest counters and posts a
# snapshot to the Team daemon. It runs in the background so a slow filesystem or
# daemon never blocks the agent turn.

payload="$(cat 2>/dev/null || true)"
if [ -z "${TEAM_USAGE_URL:-}" ] && [ -n "${TEAM_API_URL:-}" ]; then
  export TEAM_USAGE_URL="${TEAM_API_URL%/}/api/usage"
fi
[ -n "${TEAM_USAGE_URL:-}" ] || exit 0
[ -n "${TEAM_AGENT:-}" ] || exit 0
[ -n "${TEAM_ROOM:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

(
  TEAM_HOOK_PAYLOAD="$payload" python3 - <<'PY'
import glob
import json
import os
import urllib.request


def as_int(value):
    try:
        n = int(value or 0)
        return n if n > 0 else 0
    except Exception:
        return 0


def normalize_usage(raw):
    raw = raw if isinstance(raw, dict) else {}
    cached = as_int(raw.get("cached_input_tokens"))
    cached += as_int(raw.get("cache_creation_input_tokens"))
    cached += as_int(raw.get("cache_read_input_tokens"))
    out = {
        "input_tokens": as_int(raw.get("input_tokens")),
        "cached_input_tokens": cached,
        "output_tokens": as_int(raw.get("output_tokens")),
        "reasoning_output_tokens": as_int(raw.get("reasoning_output_tokens")),
        "total_tokens": as_int(raw.get("total_tokens")),
    }
    if out["total_tokens"] == 0:
        out["total_tokens"] = (
            out["input_tokens"]
            + out["cached_input_tokens"]
            + out["output_tokens"]
            + out["reasoning_output_tokens"]
        )
    return out


def default_context_window(model):
    model = (model or "").lower()
    if "claude" in model or "sonnet" in model or "opus" in model:
        return 200000
    return 0


def find_key(obj, wanted):
    if isinstance(obj, dict):
        for key, value in obj.items():
            if key.lower() in wanted and isinstance(value, str) and value:
                return value
        for value in obj.values():
            hit = find_key(value, wanted)
            if hit:
                return hit
    elif isinstance(obj, list):
        for value in obj:
            hit = find_key(value, wanted)
            if hit:
                return hit
    return ""


def load_hook_payload():
    try:
        return json.loads(os.environ.get("TEAM_HOOK_PAYLOAD", "") or "{}")
    except Exception:
        return {}


def claude_project_slug(workspace):
    return "".join(c if c.isalnum() and c.isascii() else "-" for c in workspace)


def newest_existing(paths):
    paths = [p for p in paths if p and os.path.exists(p)]
    if not paths:
        return ""
    return max(paths, key=lambda p: os.path.getmtime(p))


def claude_transcript_candidates(hook_payload):
    candidates = []
    for key in ("TEAM_TRANSCRIPT_PATH",):
        if os.environ.get(key):
            candidates.append(os.environ[key])
    payload_path = find_key(hook_payload, {"transcript_path", "transcriptpath"})
    if payload_path:
        candidates.insert(0, payload_path)
    workspace = os.environ.get("TEAM_WORKSPACE", "")
    session_id = os.environ.get("TEAM_SESSION_ID", "") or find_key(
        hook_payload, {"session_id", "sessionid"}
    )
    if workspace:
        project_dir = os.path.join(
            os.path.expanduser("~/.claude"), "projects", claude_project_slug(workspace)
        )
        if session_id:
            candidates.append(os.path.join(project_dir, f"{session_id}.jsonl"))
        candidates.extend(glob.glob(os.path.join(project_dir, "*.jsonl")))
    return candidates


def claude_report(hook_payload):
    transcript = newest_existing(claude_transcript_candidates(hook_payload))
    if not transcript:
        return None

    seen = set()
    total = {
        "input_tokens": 0,
        "cached_input_tokens": 0,
        "output_tokens": 0,
        "reasoning_output_tokens": 0,
        "total_tokens": 0,
    }
    last = None
    model = os.environ.get("TEAM_MODEL", "")
    session_id = os.environ.get("TEAM_SESSION_ID", "")
    with open(transcript, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            try:
                obj = json.loads(line)
            except Exception:
                continue
            msg = obj.get("message") if isinstance(obj, dict) else None
            if not isinstance(msg, dict) or msg.get("role") != "assistant":
                continue
            usage = msg.get("usage")
            if not isinstance(usage, dict):
                continue
            msg_id = msg.get("id") or obj.get("uuid") or obj.get("timestamp")
            if msg_id in seen:
                continue
            seen.add(msg_id)
            rec = normalize_usage(usage)
            for key in total:
                total[key] += rec[key]
            last = rec
            model = msg.get("model") or model
            session_id = obj.get("sessionId") or session_id

    if not last:
        return None
    context_window = as_int(os.environ.get("TEAM_CONTEXT_WINDOW")) or default_context_window(model)
    return {
        "backend": "claude",
        "model": model,
        "source": "claude-transcript-jsonl",
        "session_id": session_id or os.path.splitext(os.path.basename(transcript))[0],
        "total": total,
        "last": last,
        "context_tokens": last["total_tokens"],
        "context_window": context_window,
    }


def codex_session_meta(path):
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            try:
                obj = json.loads(line)
            except Exception:
                continue
            if obj.get("type") != "session_meta":
                continue
            payload = obj.get("payload") or {}
            return {
                "session_id": payload.get("session_id") or payload.get("id") or "",
                "cwd": payload.get("cwd") or "",
                "model": payload.get("model") or "",
            }
    return {"session_id": "", "cwd": "", "model": ""}


def codex_candidate_files(hook_payload):
    home = os.environ.get("CODEX_HOME") or os.path.expanduser("~/.codex")
    sessions = os.path.join(home, "sessions")
    session_id = find_key(hook_payload, {"session_id", "sessionid"})
    files = []
    if session_id:
        files.extend(glob.glob(os.path.join(sessions, "**", f"*{session_id}*.jsonl"), recursive=True))
    files.extend(glob.glob(os.path.join(sessions, "**", "*.jsonl"), recursive=True))
    seen = set()
    unique = []
    for path in sorted(files, key=lambda p: os.path.getmtime(p), reverse=True):
        if path in seen:
            continue
        seen.add(path)
        unique.append(path)
    return unique, session_id


def codex_report(hook_payload):
    files, session_id = codex_candidate_files(hook_payload)
    workspace = os.environ.get("TEAM_WORKSPACE", "")
    model = os.environ.get("TEAM_MODEL", "")
    for path in files[:80]:
        meta = codex_session_meta(path)
        if session_id and session_id not in {meta.get("session_id"), os.path.basename(path)}:
            if session_id not in os.path.basename(path):
                continue
        elif workspace and meta.get("cwd") != workspace:
            continue

        latest = None
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                try:
                    obj = json.loads(line)
                except Exception:
                    continue
                if obj.get("type") != "event_msg":
                    continue
                payload = obj.get("payload") or {}
                if payload.get("type") != "token_count":
                    continue
                latest = payload
        if not latest:
            continue
        info = latest.get("info") or {}
        last = normalize_usage(info.get("last_token_usage"))
        return {
            "backend": "codex",
            "model": model or meta.get("model", ""),
            "source": "codex-session-jsonl",
            "session_id": meta.get("session_id") or os.path.splitext(os.path.basename(path))[0],
            "total": normalize_usage(info.get("total_token_usage")),
            "last": last,
            "context_tokens": last["total_tokens"],
            "context_window": as_int(info.get("model_context_window")),
        }
    return None


def post(report):
    if not report:
        return
    data = json.dumps(report).encode("utf-8")
    req = urllib.request.Request(
        os.environ["TEAM_USAGE_URL"],
        data=data,
        headers={
            "content-type": "application/json",
            "x-agent": os.environ.get("TEAM_AGENT", ""),
            "x-room": os.environ.get("TEAM_ROOM", ""),
        },
        method="POST",
    )
    urllib.request.urlopen(req, timeout=2).read()


backend = (os.environ.get("TEAM_BACKEND") or "").lower()
hook_payload = load_hook_payload()
if backend == "claude":
    post(claude_report(hook_payload))
elif backend == "codex":
    post(codex_report(hook_payload))
PY
) >/dev/null 2>&1 &

exit 0
