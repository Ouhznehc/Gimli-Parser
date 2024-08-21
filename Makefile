compile:
	@cargo build
	@rustc -g test/test.rs -o test/test.elf

run: compile
	@target/debug/gimli-parser test/test.elf > test/gimli.out
	@llvm-dwarfdump --debug-info test/test.elf > test/llvm.out

.PHONY: compile run