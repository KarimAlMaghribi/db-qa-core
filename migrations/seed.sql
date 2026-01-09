CREATE TABLE IF NOT EXISTS users (
    id SERIAL PRIMARY KEY,
    email TEXT NOT NULL,
    name TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS orders (
    id SERIAL PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    total_cents INTEGER NOT NULL,
    status TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

INSERT INTO users (email, name) VALUES
    ('alice@example.com', 'Alice'),
    ('bob@example.com', 'Bob')
ON CONFLICT DO NOTHING;

INSERT INTO orders (user_id, total_cents, status) VALUES
    (1, 2500, 'paid'),
    (1, 1800, 'pending'),
    (2, 9900, 'paid')
ON CONFLICT DO NOTHING;
