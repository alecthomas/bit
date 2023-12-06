package engine

import (
	"fmt"
)

// fnv64a hash function.
const offset64 = 14695981039346656037
const prime64 = 1099511628211

type Hasher uint64

// HashSlice hashes a slice of strings, uint64s, byte slices or Hashers.
func HashSlice[T string | uint64 | []byte | Hasher](hasher *Hasher, slice []T) {
	switch slice := any(slice).(type) {
	case []string:
		for _, s := range slice {
			hasher.Str(s)
		}
	case []uint64:
		for _, s := range slice {
			hasher.Int(s)
		}
	case [][]byte:
		for _, s := range slice {
			hasher.Bytes(s)
		}
	case []Hasher:
		for _, s := range slice {
			hasher.Update(s)
		}
	default:
		panic(fmt.Sprintf("unexpected type %T", slice))
	}
}

func NewHasher() Hasher { return offset64 }

// Int updates the hash with a uint64.
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

// Str updates the hash with a Str.
func (h *Hasher) Str(data string) {
	f := *h
	for _, c := range data {
		f ^= Hasher(c)
		f *= prime64
	}
	*h = f
}

// Bytes updates the hash with a byte slice.
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
