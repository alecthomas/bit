package internal

import (
	"sync"
)

// Memoise a function on first use.
func Memoise[T any](f func() (T, error)) *MemoisedFunction[T] {
	return &MemoisedFunction[T]{f: f}
}

type MemoisedFunction[T any] struct {
	once sync.Once
	f    func() (T, error)
	val  T
	err  error
}

func (o *MemoisedFunction[T]) Get() (T, error) {
	o.once.Do(func() { o.val, o.err = o.f() })
	return o.val, o.err
}
