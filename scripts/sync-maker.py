"""Sync `users.maker` in Supabase to the repo's current push collaborators.

Run from CI on every push to main. `maker = true` unlocks Elite everywhere
the CLI checks the tier, so this is the one thing standing between "has push
access to anacraft" and "has every feature unlocked" -- keep it that
narrow.

Collaborators are matched to a `users` row by email, and the only email the
GitHub API will hand back for an arbitrary collaborator is whatever they have
made public on their profile. Anybody with a private email, or who has never
run `craft login`, is left alone: `sync_makers` only ever touches a row that
already exists.

The sync is total, not additive -- it hands Supabase the full list of
current collaborator emails, and `sync_makers` grants that list and revokes
everyone else. Leaving the repo revokes maker access the same run that
joining it would have granted.
"""

import json
import os
import sys
import urllib.error
import urllib.request

GITHUB_API = "https://api.github.com"


def gh(path, token):
    request = urllib.request.Request(f"{GITHUB_API}{path}")
    request.add_header("Authorization", f"Bearer {token}")
    request.add_header("Accept", "application/vnd.github+json")
    request.add_header("X-GitHub-Api-Version", "2022-11-28")
    try:
        with urllib.request.urlopen(request) as response:
            return json.load(response)
    except urllib.error.HTTPError as err:
        detail = err.read().decode(errors="replace")
        sys.exit(f"github GET {path} failed: {err.code} {detail}")


def collaborator_emails(repo, token):
    """The public email of everybody with push access to `repo`."""
    emails = []
    page = 1
    while True:
        collaborators = gh(
            f"/repos/{repo}/collaborators?affiliation=all&per_page=100&page={page}",
            token,
        )
        if not collaborators:
            return emails
        for collaborator in collaborators:
            if not collaborator.get("permissions", {}).get("push"):
                continue
            profile = gh(f"/users/{collaborator['login']}", token)
            email = profile.get("email")
            if email:
                emails.append(email)
        page += 1


def sync_makers(emails, supabase_url, service_key):
    request = urllib.request.Request(
        f"{supabase_url}/rest/v1/rpc/sync_makers",
        data=json.dumps({"p_emails": emails}).encode(),
        method="POST",
    )
    request.add_header("apikey", service_key)
    request.add_header("Authorization", f"Bearer {service_key}")
    request.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(request):
            pass
    except urllib.error.HTTPError as err:
        detail = err.read().decode(errors="replace")
        sys.exit(f"supabase rpc sync_makers failed: {err.code} {detail}")


def main():
    token = os.environ.get("GITHUB_TOKEN", "").strip()
    repo = os.environ.get("GITHUB_REPOSITORY", "").strip()
    supabase_url = os.environ.get("SUPABASE_URL", "").strip().rstrip("/")
    service_key = os.environ.get("SUPABASE_SERVICE_ROLE_KEY", "").strip()
    if not all([token, repo, supabase_url, service_key]):
        sys.exit(
            "GITHUB_TOKEN, GITHUB_REPOSITORY, SUPABASE_URL and "
            "SUPABASE_SERVICE_ROLE_KEY are all required"
        )

    emails = collaborator_emails(repo, token)
    sync_makers(emails, supabase_url, service_key)
    print(f"synced maker access for {len(emails)} collaborator email(s)")


if __name__ == "__main__":
    main()
