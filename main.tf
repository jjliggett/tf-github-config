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

resource "github_repository" "jjversion" {
  name                   = "jjversion"
  description            = "A basic SemVer versioning utility to version git projects, written in golang, using go-git."
  allow_merge_commit     = false
  allow_squash_merge     = true
  allow_rebase_merge     = true
  delete_branch_on_merge = true
  archive_on_destroy     = true
  has_discussions        = true
  has_downloads          = true
  has_issues             = true
  has_projects           = true
  has_wiki               = true
  vulnerability_alerts   = true
  topics = [
    "devops",
    "devops-tools",
    "docker",
    "git",
    "golang",
    "semver",
    "semver-parser",
    "versioning"
  ]
  security_and_analysis {
    secret_scanning {
      status = "enabled"
    }
    secret_scanning_push_protection {
      status = "enabled"
    }
  }
}

resource "github_repository" "CurrentTimeApp" {
  name                   = "CurrentTimeApp"
  description            = "A .NET 7 Blazor WASM and MAUI Blazor application that displays the time and the next time change."
  allow_merge_commit     = false
  allow_squash_merge     = true
  allow_rebase_merge     = true
  delete_branch_on_merge = true
  archive_on_destroy     = true
  has_downloads          = true
  has_issues             = true
  has_projects           = true
  has_wiki               = true
  vulnerability_alerts   = true
  topics = [
    "dotnet",
    "webassembly",
    "maui",
    "blazor",
    "blazor-wasm",
    "maui-blazor"
  ]
  pages {
    source {
      branch = "gh-pages"
      path   = "/"
    }
  }
  security_and_analysis {
    secret_scanning {
      status = "enabled"
    }
    secret_scanning_push_protection {
      status = "enabled"
    }
  }
}

locals {
  repositories = toset(["tf-github-config", "jjversion", "CurrentTimeApp"])
}

resource "github_branch_default" "tf-github-branch-default" {
  for_each   = local.repositories
  repository = each.value
  branch     = "root"
}

resource "github_branch_protection" "tf-github-default-protection" {
  for_each               = local.repositories
  repository_id          = each.value
  pattern                = "*"
  enforce_admins         = true
  allows_deletions       = true
  allows_force_pushes    = true
  require_signed_commits = true
}

resource "github_branch_protection" "tf-github-root-protection" {
  for_each                = local.repositories
  repository_id           = each.value
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
