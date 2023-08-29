package engine

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
)

type HashDB struct {
	path   string
	hashes map[string]hash
}

func NewHashDB(path string) (*HashDB, error) {
	db := &HashDB{
		path:   path,
		hashes: map[string]hash{},
	}
	r, err := os.Open(path)
	if err == nil {
		defer r.Close()
		err = json.NewDecoder(r).Decode(&db.hashes)
		if err != nil {
			return nil, fmt.Errorf("failed to decode hash database %q: %w", path, err)
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("failed to open hash database %q: %w", path, err)
	}
	return db, nil
}

func (db *HashDB) Close() error {
	w, err := os.Create(db.path + "~")
	if err != nil {
		return fmt.Errorf("failed to open hash database %q: %w", db.path, err)
	}
	defer w.Close()
	defer os.Remove(db.path + "~")
	err = json.NewEncoder(w).Encode(db.hashes)
	if err != nil {
		return fmt.Errorf("failed to encode hash database %q: %w", db.path, err)
	}
	err = w.Close()
	if err != nil {
		return fmt.Errorf("failed to close hash database %q: %w", db.path, err)
	}
	return os.Rename(db.path+"~", db.path)
}

func (db *HashDB) Get(path string) (hash, bool) {
	h, ok := db.hashes[path]
	return h, ok
}

func (db *HashDB) Set(path string, hash hash) {
	db.hashes[path] = hash
}

func (db *HashDB) Delete(text string) {
	delete(db.hashes, text)
}
