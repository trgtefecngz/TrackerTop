package main

import (
	"testing"

	"example.com/golden/store"
)

func TestLookup(t *testing.T) {
	s := store.New()
	s.Put("k", "v")
	if lookup(s, "k") != "v" {
		t.Fatal("missing")
	}
}
