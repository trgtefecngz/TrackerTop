package cache

// Cache is a read-through cache in front of a Getter.
type Cache struct {
	seen map[string]string
}

func New() *Cache {
	return &Cache{seen: map[string]string{}}
}

func (c *Cache) Get(key string) string {
	return c.seen[key]
}
