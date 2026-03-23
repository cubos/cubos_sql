# TODO

## Tipos PG adicionais no type_map builtin

Tipos que poderiam ser adicionados à tabela estática (`cubos_sql_core/src/type_map.rs`):
- `interval` (OID 1186) → `chrono::Duration` ou tipo custom
- `inet` (OID 869) / `cidr` (OID 650) → `std::net::IpAddr`
- `macaddr` (OID 829) → `[u8; 6]`
- `hstore` → `HashMap<String, Option<String>>`

## Melhorias futuras no analyzer

### Nullability

- **LEFT JOIN com WHERE que filtra NULLs**: detectar que `LEFT JOIN b ... WHERE b.col = 'x'` efetivamente vira INNER JOIN.
- **CASE exaustivo sem ELSE**: detectar cobertura completa via CHECK/enum (sem ELSE → sempre nullable hoje).
- **`lower(anyrange)` / `upper(anyrange)`**: distinguir por assinatura — compartilham nome com `lower(text)` / `upper(text)`. Hoje usa a versão text (not-null), mas a versão range pode retornar NULL.

## Limitações de nullability do analyzer estático

Cenários onde o analyzer erra e o dev precisa usar anotações `"col!"` / `"col?"` / `$param?`:

### Falsos nullable (usar `"col!"` para corrigir)

- **LEFT JOIN com WHERE que filtra NULLs**:

  ```sql
  SELECT a.id, b.name as "name!"
  FROM orders a LEFT JOIN users b ON a.user_id = b.id
  WHERE b.status = 'active'
  ```

- **CASE com cobertura exaustiva sem ELSE**:

  ```sql
  SELECT CASE status
      WHEN 'a' THEN 'Active' WHEN 'b' THEN 'Blocked' WHEN 'c' THEN 'Closed'
  END as "label!"
  FROM accounts
  ```

- **JSONB com chave garantida** (operadores `->`, `->>`, `#>`, `#>>` sempre nullable):

  ```sql
  SELECT data -> 'name' as "name!" FROM profiles
  ```

- **Funções custom (fora de pg_catalog)**:

  ```sql
  SELECT my_schema.calculate_tax(price) as "tax!" FROM products
  ```

- **Non-strict pg_catalog functions não listadas** (ex: `generate_series`):

  ```sql
  SELECT generate_series(1, 10) as "n!"
  ```

- **Coluna nullable no schema mas sempre preenchida pela app**:

  ```sql
  SELECT email as "email!" FROM users WHERE active = true
  ```

### Falsos not-null (usar `"col?"` para corrigir)

- **`lower(anyrange)` / `upper(anyrange)`** — confunde com `lower(text)`:

  ```sql
  SELECT lower(price_range) as "low?" FROM products
  ```

### O que o analyzer JÁ resolve corretamente

- **Aggregates com GROUP BY**: `SUM(not_null_col)` com `GROUP BY` → NOT NULL
- **Scalar subquery com aggregate**: `(SELECT COUNT(*) FROM ...)` → NOT NULL
- **LEFT/RIGHT/FULL JOIN**: colunas do lado nullable são nullable
- **COALESCE**: NOT NULL se qualquer argumento é NOT NULL
- **COUNT(*)**: sempre NOT NULL (com ou sem GROUP BY)
- **pg_catalog strict functions**: `length(not_null)`, `upper(not_null)` → NOT NULL
- **Non-strict functions conhecidas**: `concat()`, `now()`, `random()`, `gen_random_uuid()` → NOT NULL
- **Operadores**: `1 + 1` → NOT NULL, `nullable_col + 1` → nullable
- **Operadores JSON nullable**: `->`, `->>` → sempre nullable
- **Parâmetros**: `$foo` → NOT NULL, `$foo?` → nullable (propaga para expressões)
