package main

import (
	"fmt"

	"example.com/golden/cache"
	"example.com/golden/store"
)

// Getter is anything that returns a value for a key.
type Getter interface {
	Get(key string) string
}

func lookup(g Getter, key string) string {
	return g.Get(key)
}

func main() {
	s := store.New()
	s.Put("a", "1")
	c := cache.New()
	fmt.Println(lookup(s, "a"), lookup(c, "a"))
}
