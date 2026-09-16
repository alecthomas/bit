package users

import (
	"context"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
)

type User struct {
	ID        int64     `json:"id"`
	Name      string    `json:"name"`
	CreatedAt time.Time `json:"created_at"`
}

type Store struct {
	pool *pgxpool.Pool
}

func NewStore(pool *pgxpool.Pool) *Store {
	return &Store{pool: pool}
}

const schema = `
CREATE TABLE IF NOT EXISTS users (
    id         BIGSERIAL PRIMARY KEY,
    name       TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);`

func Migrate(ctx context.Context, pool *pgxpool.Pool) error {
	_, err := pool.Exec(ctx, schema)
	return err
}

func (s *Store) Create(ctx context.Context, name string) (User, error) {
	var u User
	err := s.pool.QueryRow(ctx,
		`INSERT INTO users (name) VALUES ($1) RETURNING id, name, created_at`,
		name,
	).Scan(&u.ID, &u.Name, &u.CreatedAt)
	return u, err
}

func (s *Store) List(ctx context.Context) ([]User, error) {
	rows, err := s.pool.Query(ctx, `SELECT id, name, created_at FROM users ORDER BY id`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := []User{}
	for rows.Next() {
		var u User
		if err := rows.Scan(&u.ID, &u.Name, &u.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, u)
	}
	return out, rows.Err()
}
