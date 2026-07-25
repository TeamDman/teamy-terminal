function Invoke-Step {
	param(
		[Parameter(Mandatory = $true)]
		[string]$Label,
		[Parameter(Mandatory = $true)]
		[scriptblock]$Action
	)

	Write-Host -ForegroundColor Yellow "Running $Label..."
	& $Action
	if ($LASTEXITCODE -ne 0) {
		throw "$Label failed with exit code $LASTEXITCODE"
	}
}

Invoke-Step -Label "format check" -Action {
	rustup run nightly -- cargo fmt --all -- --check
}

Invoke-Step -Label "clippy lint check" -Action {
	cargo clippy --workspace --all-targets --all-features -- -D warnings
}

Invoke-Step -Label "build" -Action {
	cargo build --workspace --all-features --quiet
}

Invoke-Step -Label "tests" -Action {
	cargo test --workspace --all-features --quiet
}
