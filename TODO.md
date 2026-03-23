# TODO

## Tipos PG adicionais no type_map builtin

Tipos que poderiam ser adicionados à tabela estática (`cubos_sql_core/src/type_map.rs`):
- `interval` (OID 1186) → `chrono::Duration` ou tipo custom
- `inet` (OID 869) / `cidr` (OID 650) → `std::net::IpAddr`
- `macaddr` (OID 829) → `[u8; 6]`
- `hstore` → `HashMap<String, Option<String>>`
