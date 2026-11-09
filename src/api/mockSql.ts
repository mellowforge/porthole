// Dev-only: the SQL a new query tab starts with in mock mode.
export const MOCK_QUERY_SQL = `-- Top active customers by number of orders
SELECT u.id, u.name, count(o.id) AS orders
FROM users u
LEFT JOIN orders o ON o.user_id = u.id
WHERE u.status = 'active'
GROUP BY u.id, u.name
ORDER BY orders DESC
LIMIT 20;

UPDATE users
SET status = 'inactive'
WHERE last_login < now() - interval '1 year';`;

/** Dev-only: the statement a new query tab starts with on the MongoDB mock connection. */
export const MOCK_MONGO_QUERY_SQL = `db.users.find({ status: "active" }).sort({ name: 1 })`;
