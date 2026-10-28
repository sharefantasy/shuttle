.PHONY: release-gate release-darwin-arm64 release-test docker-smoke

release-gate:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace
	cargo build --release --locked
	./target/release/shuttle --version | grep -Fx "shuttle $$(python3 release/release_version.py)"

release-darwin-arm64: release-gate
	./release/package-darwin-arm64.sh

release-test:
	./release/preflight.sh static
	python3 release/test_release_platform.py
	python3 release/test_release_tooling.py
	python3 release/test_external_tools_release.py
	python3 release/validate_workflows.py
	shellcheck release/*.sh

docker-smoke:
	@for variant in core tools full; do \
		RELEASE_TAG=v$$(python3 release/release_version.py) \
		SOURCE_COMMIT=$$(git rev-parse HEAD) DOCKER_TARGET=$$variant \
		./release/docker-release.sh load shuttle-local; \
		image=$$(python3 release/container_release.py tags --image shuttle-local --variant $$variant --version $$(python3 release/release_version.py) | head -1); \
		if [ "$$variant" = full ]; then \
			core_image=$$(docker image inspect --format '{{.Id}}' shuttle-local-core:$$(python3 release/release_version.py)); \
			tools_image=$$(docker image inspect --format '{{.Id}}' shuttle-local-tools:$$(python3 release/release_version.py)); \
			SOURCE_COMMIT=$$(git rev-parse HEAD) SHUTTLE_CORE_IMAGE=$$core_image SHUTTLE_TOOLS_IMAGE=$$tools_image SHUTTLE_TOOLS_BUNDLE_DIGEST=$$tools_image ./release/verify-container.sh $$variant $$image; \
		else \
			SOURCE_COMMIT=$$(git rev-parse HEAD) ./release/verify-container.sh $$variant $$image; \
		fi; \
	done
