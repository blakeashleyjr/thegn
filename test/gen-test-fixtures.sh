#!/usr/bin/env bash
# test/gen-test-fixtures.sh — capture REAL test-runner output per ecosystem into
# crates/superzej-host/testdata/, used as golden fixtures by the parser tests.
#
# Each ecosystem gets a minimal but REAL project (a passing + a failing test),
# the REAL tool runs (fetched ephemerally via `nix shell nixpkgs#…` when not on
# PATH), and its machine output is saved. We commit the captured OUTPUT only, not
# any project source. Re-run to refresh; ecosystems whose toolchain can't be
# fetched are reported SKIP. Test runners exit non-zero when a test fails, so we
# validate the captured file is non-empty rather than gating on exit code.
#
# Usage: test/gen-test-fixtures.sh [ecosystem...]   (default: all)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/crates/superzej-host/testdata"
mkdir -p "$OUT"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ARGS=("$@")

sel() { [ ${#ARGS[@]} -eq 0 ] || printf '%s\n' "${ARGS[@]}" | grep -qx "$1"; }
ok()   { printf '  \033[32mok\033[0m   %-8s %s (%s bytes)\n' "$1" "$2" "$(wc -c <"$OUT/$2")"; }
skip() { printf '  \033[33mSKIP\033[0m %-8s %s\n' "$1" "$2"; }
# Validate a capture is non-empty; report ok/skip.
check() { if [ -s "$OUT/$2" ]; then ok "$1" "$2"; else skip "$1" "empty $2"; fi; }

if sel rust; then
  d="$WORK/rust"; mkdir -p "$d/src"
  printf '[package]\nname="szfix"\nversion="0.1.0"\nedition="2021"\n[lib]\npath="src/lib.rs"\n' >"$d/Cargo.toml"
  printf '#[cfg(test)]\nmod tests{\n#[test] fn adds(){assert_eq!(2+2,4);}\n#[test] fn breaks(){assert_eq!(2+2,5);}\n#[test] #[ignore] fn wip(){}\n}\n' >"$d/src/lib.rs"
  (cd "$d" && NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 cargo nextest run --message-format libtest-json) >"$OUT/nextest.libtest-json" 2>/dev/null
  check rust nextest.libtest-json
fi

if sel go; then
  d="$WORK/go"; mkdir -p "$d"
  printf 'module szfix\n\ngo 1.21\n' >"$d/go.mod"
  printf 'package szfix\nfunc Add(a,b int) int { return a+b }\n' >"$d/add.go"
  printf 'package szfix\nimport "testing"\nfunc TestAdds(t *testing.T){ if Add(2,2)!=4 {t.Fatal("no")} }\nfunc TestBreaks(t *testing.T){ if Add(2,2)!=5 {t.Errorf("want 5")} }\n' >"$d/add_test.go"
  (cd "$d" && go test -v ./...) >"$OUT/go.txt" 2>/dev/null
  check go go.txt
fi

if sel pytest; then
  d="$WORK/py"; mkdir -p "$d"
  printf 'def test_adds():\n    assert 2 + 2 == 4\n\ndef test_breaks():\n    assert 2 + 2 == 5\n' >"$d/test_sample.py"
  (cd "$d" && nix shell nixpkgs#python3Packages.pytest -c pytest -v) >"$OUT/pytest.txt" 2>/dev/null
  check pytest pytest.txt
fi

if sel deno; then
  d="$WORK/deno"; mkdir -p "$d"
  printf 'Deno.test("adds", () => { if (2+2!==4) throw new Error("no"); });\nDeno.test("breaks", () => { throw new Error("boom"); });\n' >"$d/main_test.ts"
  (cd "$d" && nix shell nixpkgs#deno -c deno test --reporter junit main_test.ts) >"$OUT/deno.junit.xml" 2>/dev/null
  check deno deno.junit.xml
fi

if sel rspec; then
  d="$WORK/rb"; mkdir -p "$d/spec"
  # Avoid rspec-expectations (`expect`) so this works with rspec-core alone.
  printf 'RSpec.describe "calc" do\n  it("adds") { raise "bad" unless 2 + 2 == 4 }\n  it("breaks") { raise "bad" unless 2 + 2 == 5 }\nend\n' >"$d/spec/calc_spec.rb"
  (cd "$d" && nix shell nixpkgs#ruby nixpkgs#rubyPackages.rspec -c rspec --format json spec/calc_spec.rb) >"$OUT/rspec.json" 2>/dev/null
  check rspec rspec.json
fi

if sel zig; then
  d="$WORK/zig"; mkdir -p "$d"
  printf 'const std = @import("std");\ntest "adds" { try std.testing.expect(2+2==4); }\ntest "breaks" { try std.testing.expect(2+2==5); }\n' >"$d/main.zig"
  # zig writes results to stderr; tools are cached so no nix fetch noise.
  (cd "$d" && nix shell nixpkgs#zig -c zig test main.zig) >"$OUT/zig.txt" 2>&1
  check zig zig.txt
fi

if sel ctest; then
  d="$WORK/c"; mkdir -p "$d"
  cat >"$d/CMakeLists.txt" <<'EOF'
cmake_minimum_required(VERSION 3.16)
project(szfix NONE)
enable_testing()
add_test(NAME adds COMMAND ${CMAKE_COMMAND} -E true)
add_test(NAME breaks COMMAND ${CMAKE_COMMAND} -E false)
EOF
  ( cd "$d" && nix shell nixpkgs#cmake nixpkgs#gnumake -c bash -c 'cmake -S . -B build >/dev/null 2>&1 && cd build && ctest' ) >"$OUT/ctest.txt" 2>/dev/null
  check ctest ctest.txt
fi

if sel elixir; then
  d="$WORK/ex"; mkdir -p "$d/test"
  printf 'defmodule CalcTest do\n  use ExUnit.Case\n  test "adds" do\n    assert 2 + 2 == 4\n  end\n  test "breaks" do\n    assert 2 + 2 == 5\n  end\nend\n' >"$d/test/calc_test.exs"
  ( cd "$d" && nix shell nixpkgs#elixir -c elixir -e 'ExUnit.start()' -r test/calc_test.exs -e 'ExUnit.run()' ) >"$OUT/elixir.txt" 2>&1
  check elixir elixir.txt
fi

if sel php; then
  d="$WORK/php"; mkdir -p "$d/tests"
  cat >"$d/tests/CalcTest.php" <<'EOF'
<?php
use PHPUnit\Framework\TestCase;
final class CalcTest extends TestCase {
  public function testAdds(): void { $this->assertSame(4, 2 + 2); }
  public function testBreaks(): void { $this->assertSame(5, 2 + 2); }
}
EOF
  ( cd "$d" && nix shell nixpkgs#phpunit -c phpunit --log-junit out.xml tests/CalcTest.php >/dev/null 2>&1; cat out.xml 2>/dev/null ) >"$OUT/phpunit.junit.xml"
  check php phpunit.junit.xml
fi

# --- bats (Bash) → TAP -----------------------------------------------------
if sel bats; then
  d="$WORK/bats"; mkdir -p "$d/test"
  printf '@test "adds" { [ $((2+2)) -eq 4 ]; }\n@test "breaks" { [ $((2+2)) -eq 5 ]; }\n' >"$d/test/calc.bats"
  ( cd "$d" && nix shell nixpkgs#bats -c bats --formatter tap test ) >"$OUT/bats.tap" 2>/dev/null
  check bats bats.tap
fi

# --- perl (prove) → TAP ----------------------------------------------------
if sel perl; then
  d="$WORK/perl"; mkdir -p "$d/t"
  printf 'use Test::More tests => 2;\nok(2+2 == 4, "adds");\nok(2+2 == 5, "breaks");\n' >"$d/t/basic.t"
  ( cd "$d" && nix shell nixpkgs#perl -c prove -v t/ ) >"$OUT/perl.tap" 2>/dev/null
  check perl perl.tap
fi

# --- lua (busted) → TAP ----------------------------------------------------
if sel lua; then
  d="$WORK/lua"; mkdir -p "$d/spec"
  printf 'describe("calc", function()\n  it("adds", function() assert.is_true(2+2 == 4) end)\n  it("breaks", function() assert.is_true(2+2 == 5) end)\nend)\n' >"$d/spec/calc_spec.lua"
  ( cd "$d" && nix shell nixpkgs#luajitPackages.busted -c busted -o TAP spec ) >"$OUT/busted.tap" 2>/dev/null
  check lua busted.tap
fi

# --- gleam → text ----------------------------------------------------------
if sel gleam; then
  d="$WORK/gleam"; mkdir -p "$d/src" "$d/test"
  printf 'name = "szfix"\nversion = "1.0.0"\n\n[dependencies]\ngleam_stdlib = ">= 0.34.0 and < 2.0.0"\n\n[dev-dependencies]\ngleeunit = ">= 1.0.0 and < 2.0.0"\n' >"$d/gleam.toml"
  printf 'import gleeunit\npub fn main() { gleeunit.main() }\n' >"$d/src/szfix.gleam"
  printf 'import gleeunit/should\npub fn adds_test() { should.equal(2 + 2, 4) }\npub fn breaks_test() { should.equal(2 + 2, 5) }\n' >"$d/test/szfix_test.gleam"
  ( cd "$d" && nix shell nixpkgs#gleam nixpkgs#erlang nixpkgs#rebar3 -c gleam test ) >"$OUT/gleam.txt" 2>&1
  check gleam gleam.txt
fi

# --- dart → JSON -----------------------------------------------------------
if sel dart; then
  d="$WORK/dart"; mkdir -p "$d/test"
  printf 'name: szfix\nenvironment:\n  sdk: ">=3.0.0 <4.0.0"\ndev_dependencies:\n  test: any\n' >"$d/pubspec.yaml"
  printf "import 'package:test/test.dart';\nvoid main() {\n  test('adds', () => expect(2 + 2, 4));\n  test('breaks', () => expect(2 + 2, 5));\n}\n" >"$d/test/calc_test.dart"
  ( cd "$d" && nix shell nixpkgs#dart -c bash -c 'dart pub get >/dev/null 2>&1 && dart test --reporter json' ) >"$OUT/dart.json" 2>/dev/null
  check dart dart.json
fi

# --- dotnet / .NET → TRX ---------------------------------------------------
if sel dotnet; then
  d="$WORK/net"; mkdir -p "$d"
  cat >"$d/szfix.csproj" <<'EOF'
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup><TargetFramework>net8.0</TargetFramework><Nullable>enable</Nullable></PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Microsoft.NET.Test.Sdk" Version="17.*" />
    <PackageReference Include="xunit" Version="2.*" />
    <PackageReference Include="xunit.runner.visualstudio" Version="2.*" />
  </ItemGroup>
</Project>
EOF
  printf 'using Xunit;\npublic class CalcTest {\n  [Fact] public void Adds() => Assert.Equal(4, 2 + 2);\n  [Fact] public void Breaks() => Assert.Equal(5, 2 + 2);\n}\n' >"$d/CalcTest.cs"
  ( cd "$d" && nix shell nixpkgs#dotnet-sdk -c bash -c 'dotnet test --logger "trx;LogFileName=sz.trx" >/dev/null 2>&1; cat TestResults/sz.trx' ) >"$OUT/dotnet.trx" 2>/dev/null
  check dotnet dotnet.trx
fi

# --- gleam → text ----------------------------------------------------------
if sel gleam; then
  d="$WORK/gleam"; mkdir -p "$d/src" "$d/test"
  printf 'name = "szfix"\nversion = "1.0.0"\n\n[dependencies]\ngleam_stdlib = ">= 0.34.0 and < 2.0.0"\n\n[dev-dependencies]\ngleeunit = ">= 1.0.0 and < 2.0.0"\n' >"$d/gleam.toml"
  printf 'import gleeunit\npub fn main() { gleeunit.main() }\n' >"$d/src/szfix.gleam"
  printf 'import gleeunit/should\npub fn adds_test() { should.equal(2 + 2, 4) }\npub fn breaks_test() { should.equal(2 + 2, 5) }\n' >"$d/test/szfix_test.gleam"
  ( cd "$d" && nix shell nixpkgs#gleam nixpkgs#erlang nixpkgs#rebar3 -c gleam test ) >"$OUT/gleam.txt" 2>&1
  check gleam gleam.txt
fi

# --- ocaml / dune → text ---------------------------------------------------
if sel ocaml; then
  d="$WORK/ml"; mkdir -p "$d/test"
  printf '(lang dune 3.0)\n' >"$d/dune-project"
  printf '(test (name test_calc))\n' >"$d/test/dune"
  printf 'let () =\n  assert (2 + 2 = 4);\n  Printf.printf "adds ok\\n";\n  assert (2 + 2 = 5)\n' >"$d/test/test_calc.ml"
  ( cd "$d" && nix shell nixpkgs#dune_3 nixpkgs#ocaml -c dune runtest ) >"$OUT/ocaml.txt" 2>&1
  check ocaml ocaml.txt
fi

# --- java / maven → JUnit XML ----------------------------------------------
if sel maven; then
  d="$WORK/mvn"; mkdir -p "$d/src/test/java"
  cat >"$d/pom.xml" <<'EOF'
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>sz</groupId><artifactId>szfix</artifactId><version>1.0</version>
  <properties><maven.compiler.release>17</maven.compiler.release></properties>
  <dependencies>
    <dependency><groupId>org.junit.jupiter</groupId><artifactId>junit-jupiter</artifactId><version>5.10.2</version><scope>test</scope></dependency>
  </dependencies>
  <build><plugins>
    <plugin><groupId>org.apache.maven.plugins</groupId><artifactId>maven-surefire-plugin</artifactId><version>3.2.5</version></plugin>
  </plugins></build>
</project>
EOF
  cat >"$d/src/test/java/CalcTest.java" <<'EOF'
import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;
class CalcTest {
  @Test void adds() { assertEquals(4, 2 + 2); }
  @Test void breaks() { assertEquals(5, 2 + 2); }
}
EOF
  ( cd "$d" && nix shell nixpkgs#maven nixpkgs#jdk17 -c bash -c 'mvn -q -o test >/dev/null 2>&1; cat target/surefire-reports/*.xml 2>/dev/null || mvn -q test >/dev/null 2>&1; cat target/surefire-reports/*.xml 2>/dev/null' ) >"$OUT/maven.junit.xml" 2>/dev/null
  check maven maven.junit.xml
fi

# --- swift → text (XCTest) -------------------------------------------------
if sel swift; then
  d="$WORK/sw"; mkdir -p "$d/Sources/szfix" "$d/Tests/szfixTests"
  cat >"$d/Package.swift" <<'EOF'
// swift-tools-version:5.9
import PackageDescription
let package = Package(name: "szfix",
  targets: [ .target(name: "szfix"), .testTarget(name: "szfixTests", dependencies: ["szfix"]) ])
EOF
  printf 'public func add(_ a: Int, _ b: Int) -> Int { a + b }\n' >"$d/Sources/szfix/szfix.swift"
  cat >"$d/Tests/szfixTests/CalcTests.swift" <<'EOF'
import XCTest
@testable import szfix
final class CalcTests: XCTestCase {
  func testAdds() { XCTAssertEqual(add(2, 2), 4) }
  func testBreaks() { XCTAssertEqual(add(2, 2), 5) }
}
EOF
  ( cd "$d" && nix shell nixpkgs#swift -c swift test ) >"$OUT/swift.txt" 2>&1
  check swift swift.txt
fi

echo "fixtures in $OUT:"
ls -1 "$OUT"
