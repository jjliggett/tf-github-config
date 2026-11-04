# cairn/upstream holds third-party submodules (with deliberately malformed
# HCL test fixtures), so format the root module and our own directories
# explicitly instead of recursing over the whole tree.
format:
	terraform fmt
	terraform fmt --recursive cairn/bench

format-check:
	terraform fmt -check
	terraform fmt -check --recursive cairn/bench

validate:
	terraform validate
