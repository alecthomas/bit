import { useEffect, useState } from "react";

export interface User {
  id: number;
  name: string;
  created_at: string;
}

export function App() {
  const [users, setUsers] = useState<User[]>([]);
  const [name, setName] = useState("");

  async function refresh() {
    const res = await fetch("/api/users");
    setUsers(await res.json());
  }

  useEffect(() => {
    refresh().catch(() => {});
  }, []);

  async function add(e: React.FormEvent) {
    e.preventDefault();
    if (!name.trim()) return;
    await fetch("/api/users", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name }),
    });
    setName("");
    await refresh();
  }

  return (
    <main>
      <h1>Users</h1>
      <form onSubmit={add}>
        <input
          aria-label="name"
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="name"
        />
        <button type="submit">add</button>
      </form>
      <ul>
        {users.map((u) => (
          <li key={u.id}>{u.name}</li>
        ))}
      </ul>
    </main>
  );
}
