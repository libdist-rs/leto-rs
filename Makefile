.PHONY: all

all:
	cargo b --all

format:
	@cargo +nightly fmt --all

update-config:
	@cargo r -p node -- config -n 4 -o examples
	@cargo r -p node -- keys -n 4 -o examples

logs-clean:
	@sed -i '' '/Got a transaction/d' test-log*.log
	@sed -i '' '/Async Sending Tx \[/d' test-log-client.log

test-run:
	@timeout 10 bash scripts/test4nodes.sh
	@sleep 1
	@make logs-clean || true
	@make clean || true

clean:
	@echo "Cleaning DB files"
	@# https://www.baeldung.com/linux/find-exec-command
	@find . -name "db-*.db" -exec rm -rf {} \;
