package users

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
)

func newPool(t *testing.T) *pgxpool.Pool {
	t.Helper()
	dsn := os.Getenv("DATABASE_URL")
	if dsn == "" {
		dsn = "postgres://postgres:postgres@localhost:5432/app?sslmode=disable"
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	pool, err := pgxpool.New(ctx, dsn)
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	t.Cleanup(pool.Close)
	if err := Migrate(ctx, pool); err != nil {
		t.Fatalf("migrate: %v", err)
	}
	if _, err := pool.Exec(ctx, "TRUNCATE users RESTART IDENTITY"); err != nil {
		t.Fatalf("truncate: %v", err)
	}
	return pool
}

func TestCreateAndList(t *testing.T) {
	pool := newPool(t)
	store := NewStore(pool)
	ctx := context.Background()

	for _, name := range []string{"ada", "grace", "linus"} {
		if _, err := store.Create(ctx, name); err != nil {
			t.Fatalf("create %q: %v", name, err)
		}
	}

	list, err := store.List(ctx)
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(list) != 3 {
		t.Fatalf("expected 3 users, got %d", len(list))
	}
	want := []string{"ada", "grace", "linus"}
	for i, u := range list {
		if u.Name != want[i] {
			t.Errorf("user %d: got %q want %q", i, u.Name, want[i])
		}
	}
}
