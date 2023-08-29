package engine

import (
	"fmt"
)

// fnv64a hash function.
const offset64 = 14695981039346656037
const prime64 = 1099511628211

type hasher uint64

func newHasher() hasher { return offset64 }

// Update the hash with a uint64.
func (h *hasher) int(data uint64) {
	f := *h
	f ^= hasher(data)
	f *= prime64
	*h = f
}

// Update the hash with another hash.
func (h *hasher) update(other hasher) {
	f := *h
	f ^= other
	f *= prime64
	*h = f
}

// Update the hash with a string.
func (h *hasher) string(data string) {
	f := *h
	for _, c := range data {
		f ^= hasher(c)
		f *= prime64
	}
	*h = f
}

// Update the hash with a byte slice.
func (h *hasher) bytes(data []byte) {
	f := *h
	for _, c := range data {
		f ^= hasher(c)
		f *= prime64
	}
	*h = f
}

func (h *hasher) String() string {
	return fmt.Sprintf("%x", uint64(*h))
}
