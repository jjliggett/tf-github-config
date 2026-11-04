# Benchmark fixture: no providers to download, so a run measures the
# orchestrator (queueing, workspace setup, state handling, log streaming)
# rather than the network. terraform_data is built into OpenTofu >= 1.6.
variable "resource_count" {
  type    = number
  default = 100
}

resource "terraform_data" "item" {
  count = var.resource_count
  input = "item-${count.index}"
}

output "items" {
  value = length(terraform_data.item)
}
