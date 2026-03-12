#!/usr/bin/env python3
import base64
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


REPO_OWNER = os.environ.get("SYMPHONY_SMOKE_REPO_OWNER", "kalepail")
REPO_NAME = os.environ.get("SYMPHONY_SMOKE_REPO_NAME", "symphony-smoke-lab")
REPO_BRANCH = os.environ.get("SYMPHONY_SMOKE_REPO_BRANCH", "main")
LINEAR_PROJECT_NAME_PREFIX = os.environ.get(
    "SYMPHONY_LINEAR_LIVE_E2E_PREFIX", "Symphony Live E2E"
)
TODOIST_LIVE_E2E_PROJECT_PREFIXES = [
    os.environ.get(
        "SYMPHONY_TODOIST_LIVE_E2E_PREFIX", "Symphony Rust Todoist Live E2E"
    ),
    os.environ.get(
        "SYMPHONY_TODOIST_SMOKE_E2E_PREFIX", "Symphony Rust Todoist Smoke E2E"
    ),
]
TODOIST_LIVE_E2E_PROJECT_PREFIXES = [
    prefix for prefix in TODOIST_LIVE_E2E_PROJECT_PREFIXES if prefix
]
TODOIST_BASE_URL_DEFAULT = "https://api.todoist.com/api/v1"
SMOKE_BASELINE_FILES = (
    "AGENTS.md",
    "SMOKE_TARGET.md",
    ".codex/skills/commit/SKILL.md",
    ".codex/skills/pull/SKILL.md",
    ".codex/skills/push/SKILL.md",
    ".codex/skills/land/SKILL.md",
    ".codex/skills/land/land_watch.py",
    "smoke/review-target.md",
)
HTTP_RETRY_MAX_RETRIES = 4
HTTP_RETRY_DEFAULT_DELAY_SECS = 2
HTTP_RETRY_MAX_DELAY_SECS = 60
HTTP_RETRY_MAX_TOTAL_WAIT_SECS = 300


def env_flag(name: str) -> bool:
    value = os.environ.get(name, "")
    return value.strip().lower() in {"1", "true", "yes", "on"}


def load_env_file(path: Path) -> None:
    if not path.is_file():
        return
    for raw_line in path.read_text().splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        value = value.strip()
        if not key or key in os.environ:
            continue
        if value and value[0] == value[-1] and value[0] in ("'", '"'):
            value = value[1:-1]
        os.environ[key] = value


def load_default_envs(repo_root: Path) -> None:
    for relative in (
        "rust-todoist/.env",
        "rust-todoist/.env.local",
        "elixir/.env",
        "elixir/.env.local",
    ):
        load_env_file(repo_root / relative)


def remove_legacy_active_smoke_registry(repo_root: Path) -> None:
    legacy_path = repo_root / "tmp" / "active_smoke_projects.json"
    if not legacy_path.exists():
        return
    legacy_path.unlink()
    print(f"Removed legacy active smoke registry {legacy_path}")


def transient_retry_delay(attempt: int) -> int:
    power = min(attempt, 6)
    return max(
        1,
        min(HTTP_RETRY_MAX_DELAY_SECS, HTTP_RETRY_DEFAULT_DELAY_SECS * (2**power)),
    )


def http_retry_after_seconds(error_body: str, headers) -> int:
    header_hint = None
    retry_after = headers.get("Retry-After")
    if retry_after:
        try:
            header_hint = int(retry_after)
        except ValueError:
            pass
    body_hint = None
    try:
        payload = json.loads(error_body)
        retry_after = payload.get("retry_after")
        if isinstance(retry_after, int):
            body_hint = retry_after
        retry_after = payload.get("error_extra", {}).get("retry_after")
        if isinstance(retry_after, int):
            body_hint = retry_after
    except json.JSONDecodeError:
        pass
    if header_hint is not None and body_hint is not None:
        return max(header_hint, body_hint)
    if header_hint is not None:
        return header_hint
    if body_hint is not None:
        return body_hint
    return HTTP_RETRY_DEFAULT_DELAY_SECS


def http_error_mentions_rate_limit(error_body: str) -> bool:
    lower = error_body.lower()
    return (
        "rate limit" in lower
        or "secondary rate limit" in lower
        or "abuse" in lower
    )


def http_status_is_transient(code: int, error_body: str) -> bool:
    return code in {408, 429, 500, 502, 503, 504} or (
        code == 403 and http_error_mentions_rate_limit(error_body)
    )


def http_retry_delay_seconds(code, error_body: str, headers, attempt: int, waited_secs: int):
    if attempt >= HTTP_RETRY_MAX_RETRIES:
        return None

    if code is None:
        delay = transient_retry_delay(attempt)
    elif code == 429 or (code == 403 and http_error_mentions_rate_limit(error_body)):
        delay = max(
            1,
            min(
                HTTP_RETRY_MAX_DELAY_SECS,
                http_retry_after_seconds(error_body, headers),
            ),
        )
    elif http_status_is_transient(code, error_body):
        delay = transient_retry_delay(attempt)
    else:
        return None

    if waited_secs + delay > HTTP_RETRY_MAX_TOTAL_WAIT_SECS:
        return None

    return delay


def http_request_json(url: str, *, method: str = "GET", headers=None, body=None):
    waited_secs = 0
    for attempt in range(HTTP_RETRY_MAX_RETRIES + 1):
        request = urllib.request.Request(
            url,
            method=method,
            headers=headers or {},
            data=None if body is None else json.dumps(body).encode("utf-8"),
        )
        if body is not None:
            request.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(request) as response:
                payload = response.read().decode("utf-8")
                return json.loads(payload) if payload else None
        except urllib.error.HTTPError as error:
            payload = error.read().decode("utf-8")
            delay = http_retry_delay_seconds(
                error.code, payload, error.headers, attempt, waited_secs
            )
            if delay is not None:
                print(f"HTTP {error.code} for {url}; retrying in {delay}s")
                waited_secs += delay
                time.sleep(delay)
                continue
            raise
        except urllib.error.URLError as error:
            delay = http_retry_delay_seconds(None, "", {}, attempt, waited_secs)
            if delay is not None:
                print(f"Transport error for {url}: {error}; retrying in {delay}s")
                waited_secs += delay
                time.sleep(delay)
                continue
            raise


def github_token() -> str:
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        return token
    result = subprocess.run(
        ["gh", "auth", "token"],
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def github_api(path: str, *, method: str = "GET", body=None):
    token = github_token()
    url = f"https://api.github.com{path}"
    return http_request_json(
        url,
        method=method,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
        },
        body=body,
    )


def github_branches():
    page = 1
    branches = []

    while True:
        payload = github_api(
            f"/repos/{REPO_OWNER}/{REPO_NAME}/branches?per_page=100&page={page}"
        )
        if not payload:
            break
        branches.extend(payload)
        if len(payload) < 100:
            break
        page += 1

    return branches


def reset_github_repo(repo_root: Path) -> None:
    baseline_root = repo_root / "scripts" / "smoke_repo_baseline"
    baseline_files = {
        repo_path: baseline_root / repo_path for repo_path in SMOKE_BASELINE_FILES
    }

    pulls = github_api(f"/repos/{REPO_OWNER}/{REPO_NAME}/pulls?state=open&per_page=100")
    for pull in pulls:
        github_api(
            f"/repos/{REPO_OWNER}/{REPO_NAME}/pulls/{pull['number']}",
            method="PATCH",
            body={"state": "closed"},
        )
        print(f"Closed smoke PR #{pull['number']}")

    for branch in github_branches():
        branch_name = branch.get("name")
        if not branch_name or branch_name == REPO_BRANCH:
            continue
        github_api(
            f"/repos/{REPO_OWNER}/{REPO_NAME}/git/refs/heads/{urllib.parse.quote(branch_name, safe='')}",
            method="DELETE",
        )
        print(f"Deleted smoke branch {branch_name}")

    for repo_path, local_path in baseline_files.items():
        content = local_path.read_text()
        existing = None
        try:
            existing = github_api(
                f"/repos/{REPO_OWNER}/{REPO_NAME}/contents/{repo_path}?ref={REPO_BRANCH}"
            )
            current = base64.b64decode(existing["content"]).decode("utf-8")
            if current == content:
                print(f"GitHub baseline already clean for {repo_path}")
                continue
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
        github_api(
            f"/repos/{REPO_OWNER}/{REPO_NAME}/contents/{repo_path}",
            method="PUT",
            body={
                "message": f"Reset {repo_path} smoke baseline",
                "content": base64.b64encode(content.encode("utf-8")).decode("ascii"),
                "branch": REPO_BRANCH,
                **({"sha": existing["sha"]} if existing else {}),
            },
        )
        print(f"Reset GitHub baseline for {repo_path}")


def protected_todoist_project_ids(repo_root: Path) -> set[str]:
    protected: set[str] = set()

    for raw_value in os.environ.get("SYMPHONY_SMOKE_PROTECT_PROJECT_IDS", "").split(","):
        value = raw_value.strip()
        if value:
            protected.add(value)

    return protected


def todoist_request(path: str, *, method: str = "GET", query=None, body=None):
    token = os.environ.get("TODOIST_API_TOKEN")
    if not token:
        raise RuntimeError("TODOIST_API_TOKEN is required for Todoist cleanup")

    base_url = os.environ.get("TODOIST_API_BASE_URL", TODOIST_BASE_URL_DEFAULT).rstrip("/")
    url = f"{base_url}{path}"
    if query:
        url = f"{url}?{urllib.parse.urlencode(query)}"

    data = None if body is None else json.dumps(body).encode("utf-8")
    request_id = None
    if method in {"POST", "DELETE"}:
        request_id = f"todoist-smoke-reset-{time.time_ns()}"

    waited_secs = 0
    for attempt in range(HTTP_RETRY_MAX_RETRIES + 1):
        request = urllib.request.Request(
            url,
            method=method,
            headers={
                "Authorization": f"Bearer {token}",
                "Accept": "application/json",
                **({"Content-Type": "application/json"} if body is not None else {}),
                **({"X-Request-Id": request_id} if request_id else {}),
            },
            data=data,
        )
        try:
            with urllib.request.urlopen(request) as response:
                payload = response.read().decode("utf-8")
                return json.loads(payload) if payload else None
        except urllib.error.HTTPError as error:
            payload = error.read().decode("utf-8")
            if error.code == 404:
                return None
            delay = http_retry_delay_seconds(
                error.code, payload, error.headers, attempt, waited_secs
            )
            if delay is not None:
                print(f"Todoist request failed on {path} with HTTP {error.code}; retrying in {delay}s")
                waited_secs += delay
                time.sleep(delay)
                continue
            raise RuntimeError(f"Todoist request failed ({error.code}) for {path}: {payload}") from error
        except urllib.error.URLError as error:
            delay = http_retry_delay_seconds(None, "", {}, attempt, waited_secs)
            if delay is not None:
                print(f"Todoist transport error on {path}: {error}; retrying in {delay}s")
                waited_secs += delay
                time.sleep(delay)
                continue
            raise RuntimeError(f"Todoist request failed for {path}: {error}") from error


def todoist_paginated_results(path: str, *, query=None):
    cursor = None
    results = []

    while True:
        page_query = {"limit": "200", **(query or {})}
        if cursor:
            page_query["cursor"] = cursor
        payload = todoist_request(path, query=page_query)
        if not isinstance(payload, dict):
            raise RuntimeError(f"Todoist list request for {path} returned unexpected payload: {payload}")
        results.extend(payload.get("results", []))
        cursor = payload.get("next_cursor")
        if not cursor:
            break

    return results


def reset_todoist_project(repo_root: Path) -> None:
    protected_project_ids = protected_todoist_project_ids(repo_root)
    project_id = os.environ.get("SYMPHONY_SMOKE_PROJECT_ID")
    if not project_id:
        print("Skipping Todoist cleanup; SYMPHONY_SMOKE_PROJECT_ID is unset")
    elif project_id in protected_project_ids:
        print(f"Skipping Todoist task cleanup for active protected project {project_id}")
    else:
        for task in todoist_paginated_results("/tasks", query={"project_id": project_id}):
            task_id = str(task["id"])
            todoist_request(f"/tasks/{task_id}", method="DELETE")
            print(f"Deleted Todoist task {task_id} ({task.get('content', 'untitled')})")

    projects = todoist_paginated_results("/projects")
    for project in projects:
        name = project.get("name", "")
        if not any(name.startswith(prefix) for prefix in TODOIST_LIVE_E2E_PROJECT_PREFIXES):
            continue
        project_id = str(project["id"])
        if project_id in protected_project_ids:
            print(f"Skipping active protected Todoist live e2e project {project_id} ({name})")
            continue
        todoist_request(f"/projects/{project_id}", method="DELETE")
        print(f"Deleted Todoist live e2e project {project_id} ({name})")


def linear_request(query: str, variables=None):
    token = os.environ.get("LINEAR_API_KEY")
    if not token:
        raise RuntimeError("LINEAR_API_KEY is required for Linear cleanup")
    return http_request_json(
        "https://api.linear.app/graphql",
        method="POST",
        headers={"Authorization": token},
        body={"query": query, "variables": variables or {}},
    )


def reset_linear_projects() -> None:
    token = os.environ.get("LINEAR_API_KEY")
    if not token:
        print("Skipping Linear cleanup; LINEAR_API_KEY is unset")
        return

    query = """
    query CleanupProjects($term: String!) {
      projects(first: 100, filter: { name: { containsIgnoreCase: $term } }) {
        nodes {
          id
          name
        }
      }
    }
    """
    payload = linear_request(query, {"term": LINEAR_PROJECT_NAME_PREFIX})
    projects = payload.get("data", {}).get("projects", {}).get("nodes", [])

    mutation = """
    mutation DeleteProject($id: String!) {
      projectDelete(id: $id) {
        success
      }
    }
    """
    for project in projects:
        response = linear_request(mutation, {"id": project["id"]})
        success = (
            response.get("data", {})
            .get("projectDelete", {})
            .get("success", False)
        )
        if not success:
            raise RuntimeError(f"Failed to delete Linear project {project['id']}: {response}")
        print(f"Deleted Linear project {project['name']} ({project['id']})")


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    load_default_envs(repo_root)
    remove_legacy_active_smoke_registry(repo_root)
    if env_flag("SYMPHONY_SMOKE_SKIP_GITHUB_RESET"):
        print("Skipping GitHub cleanup; SYMPHONY_SMOKE_SKIP_GITHUB_RESET is set")
    else:
        reset_github_repo(repo_root)
    if env_flag("SYMPHONY_SMOKE_SKIP_TODOIST_RESET"):
        print("Skipping Todoist cleanup; SYMPHONY_SMOKE_SKIP_TODOIST_RESET is set")
    else:
        reset_todoist_project(repo_root)
    if env_flag("SYMPHONY_SMOKE_SKIP_LINEAR_RESET"):
        print("Skipping Linear cleanup; SYMPHONY_SMOKE_SKIP_LINEAR_RESET is set")
    else:
        reset_linear_projects()
    return 0


if __name__ == "__main__":
    sys.exit(main())
