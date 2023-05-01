resource "github_repository" "tf-github-config" {
  name                   = "tf-github-config"
  description            = "GitHub configuration using Terraform"
  allow_merge_commit     = false
  allow_squash_merge     = true
  allow_rebase_merge     = true
  delete_branch_on_merge = true
  archive_on_destroy     = true
  security_and_analysis {
    secret_scanning {
      status = "enabled"
    }
    secret_scanning_push_protection {
      status = "enabled"
    }
  }
}

resource "github_branch_default" "tf-github-branch-default" {
  repository = github_repository.tf-github-config.name
  branch     = "root"
}

resource "github_branch_protection" "tf-github-default-protection" {
  repository_id          = github_repository.tf-github-config.name
  pattern                = "*"
  enforce_admins         = true
  allows_deletions       = true
  allows_force_pushes    = true
  require_signed_commits = true
}

resource "github_branch_protection" "tf-github-root-protection" {
  repository_id           = github_repository.tf-github-config.name
  pattern                 = "root"
  enforce_admins          = false
  allows_deletions        = false
  allows_force_pushes     = false
  require_signed_commits  = true
  required_linear_history = true
  required_pull_request_reviews {
    dismiss_stale_reviews           = true
    required_approving_review_count = 1
    require_last_push_approval      = true
  }
}
