package engine

import (
	"fmt"
)

// fnv64a hash function.
const offset64 = 14695981039346656037
const prime64 = 1099511628211

type Hasher uint64

func NewHasher() Hasher { return offset64 }

// Update the hash with a uint64.
func (h *Hasher) Int(data uint64) {
	f := *h
	f ^= Hasher(data)
	f *= prime64
	*h = f
}

// Update the hash with another hash.
func (h *Hasher) Update(other Hasher) {
	f := *h
	f ^= other
	f *= prime64
	*h = f
}

// Update the hash with a string.
func (h *Hasher) string(data string) {
	f := *h
	for _, c := range data {
		f ^= Hasher(c)
		f *= prime64
	}
	*h = f
}

// Update the hash with a byte slice.
func (h *Hasher) Bytes(data []byte) {
	f := *h
	for _, c := range data {
		f ^= Hasher(c)
		f *= prime64
	}
	*h = f
}

func (h *Hasher) String() string {
	return fmt.Sprintf("%x", uint64(*h))
}
