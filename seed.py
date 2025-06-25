#!/usr/bin/env python3
"""
Bors Seed Script

This script creates multiple PRs and approves them with `@bors r+` comments.

Usage:
    python seed.py --repo owner/repo-name --token $GITHUB_TOKEN --count 5

Requirements:
    pip install PyGithub
"""

import argparse
import sys
import time
from github import Github, Auth
from github.GithubException import GithubException

def create_pr(repo, index):
    branch_name = f"test-pr-{index}-{int(time.time())}"

    # Get the default branch
    default_branch = repo.default_branch
    base_branch = repo.get_branch(default_branch)

    # Create new branch
    repo.create_git_ref(
        ref=f"refs/heads/{branch_name}",
        sha=base_branch.commit.sha
    )

    # Create markdown file
    content = f"# Test File {index}\n\nGenerated at: {time.strftime('%Y-%m-%d %H:%M:%S')}\n"
    repo.create_file(
        path=f"test-file-{index}.md",
        message=f"Test change {index}: Add test file",
        content=content,
        branch=branch_name
    )

    # Create pull request
    pr = repo.create_pull(
        title=f"Test PR {index}: Automated test change",
        body="This is an automated test PR.",
        head=branch_name,
        base=default_branch
    )

    return pr.number


def post_comment(repo, pr_number, comment):
    issue = repo.get_issue(pr_number)
    issue.create_comment(comment)


def main():
    parser = argparse.ArgumentParser(description="Create test PRs and approve them with bors")
    parser.add_argument("--repo", required=True, help="Repository in format owner/repo-name")
    parser.add_argument("--token", required=True, help="GitHub personal access token")
    parser.add_argument("--count", type=int, default=3, help="Number of PRs to create (default: 3)")

    args = parser.parse_args()

    # Initialize GH client
    try:
        auth = Auth.Token(args.token)
        g = Github(auth=auth)
        repo = g.get_repo(args.repo)
        print(f"Connected to repository: {repo.full_name}")
    except GithubException as e:
        print(f"Error connecting to GitHub: {e}")
        sys.exit(1)

    pr_numbers = []

    # Create PRs
    print(f"\nCreating {args.count} PRs...")
    for i in range(1, args.count + 1):
        try:
            pr_number = create_pr(repo, i)
            pr_numbers.append(pr_number)
            print(f"✓ Created PR #{pr_number}")
        except GithubException as e:
            print(f"✗ Failed to create PR {i}: {e}")

    if not pr_numbers:
        print("No PRs were created successfully.")
        sys.exit(1)

    # Approve all PRs
    print(f"\nApproving {len(pr_numbers)} PRs with `@bors r+`...")
    for pr_number in pr_numbers:
        try:
            issue = repo.get_issue(pr_number)
            issue.create_comment("@bors r+")

            print(f"✓ Approved PR #{pr_number}")
        except GithubException as e:
            print(f"✗ Failed to approve PR #{pr_number}: {e}")

    print(f"\n{len(pr_numbers)} PRs:")

    for pr_number in pr_numbers:
        print(f" - PR #{pr_number}: https://github.com/{args.repo}/pull/{pr_number}")

if __name__ == "__main__":
    main()
