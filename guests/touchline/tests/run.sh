#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
compiler=${OCAMLC:-ocamlc}
build=$(mktemp -d)
trap 'rm -rf "$build"' EXIT
cd "$build"

"$compiler" -c -o match.cmo "$project_dir/src/match.ml"
"$compiler" -c -o game_test.cmo "$project_dir/tests/game_test.ml"
"$compiler" -o game_tests match.cmo game_test.cmo
./game_tests

"$compiler" -c -o playbit.cmo "$project_dir/tests/mock_playbit.ml"
"$compiler" -c -o wire.cmo "$project_dir/src/wire.ml"
"$compiler" -c -o session.cmo "$project_dir/src/session.ml"
"$compiler" -c -o session_test.cmo "$project_dir/tests/session_test.ml"
"$compiler" -o session_tests playbit.cmo match.cmo wire.cmo session.cmo session_test.cmo
./session_tests

"$compiler" -c -o scorebook.cmo "$project_dir/src/scorebook.ml"
"$compiler" -c -o scorebook_test.cmo "$project_dir/tests/scorebook_test.ml"
"$compiler" -o scorebook_tests playbit.cmo match.cmo scorebook.cmo scorebook_test.cmo
./scorebook_tests

"$compiler" -c -o pixel_layout.cmo "$project_dir/src/pixel_layout.ml"
"$compiler" -c -o pixel_layout_test.cmo "$project_dir/tests/pixel_layout_test.ml"
"$compiler" -o pixel_layout_tests pixel_layout.cmo pixel_layout_test.cmo
./pixel_layout_tests
