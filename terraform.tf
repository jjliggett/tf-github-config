terraform {

  cloud {
    organization = "jjliggett"

    workspaces {
      name = "tf-github-config"
    }
  }

  required_providers {
    github = {
      source  = "integrations/github"
      version = "~> 5.0"
    }
  }
}
