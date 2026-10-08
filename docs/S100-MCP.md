# Native S-100 MCP

FerriteS100 statically links `crates/ferrite-mcp`. The previous feature branch's external S-101 Explorer plugin and signature bypass are not required. Open **Help → S-100 MCP**, enable the service, then copy its endpoint. The listener binds an OS-selected loopback port. Stopping it revokes process-local credentials.

## Products and tools

`datasets_list` enumerates the committed loaded dataset tree with explicit identities, product names and the host's FC/PC metadata. `dataset_metadata` and `s100_capabilities` are product-independent. Other tools use the S-101 adapter: `catalogue_search`, `catalogue_describe_feature`, `catalogue_describe_attribute`, `feature_get`, `feature_query_by_type`, `feature_query_bbox`, `feature_nearby`, `feature_count`. Supply `dataset_id` from `datasets_list` whenever multiple datasets are present; feature IDs are local to that dataset.

Other products expose metadata when present in the host tree. S-102 coverage reads and S-421 route geometry are not yet query adapters; unsupported queries return explicit errors. An S-101 dataset that has no compatible active FC/PC also exposes metadata only. This is an S-100-oriented MCP interface, not a claim that every S-100 product or S-100 requirement is implemented.

Indexing uses a copy of loaded, updated cell state rather than reopening a base `.000` file. Catalogue typing is applied to that query copy only. Index construction runs on one background worker, shares the FC, and discards superseded results. Load/unload/catalogue-tree changes invalidate published snapshots. The protocol has no chart, portrayal, selection or route mutation tools.

## Connecting a client

The service implements JSON responses for Streamable HTTP (2025-06-18), OAuth discovery, dynamic public-client registration and authorization-code exchange with PKCE S256. Clients must send the resource URL in authorization and token requests. Use the **Copy approval code** button in the native window and paste that code into the authorization page only for a client you chose. A network caller cannot approve itself without this desktop-owner secret. The secret is not returned by discovery or dataset tools. Registered callbacks are exact matches, with only the port allowed to vary for the same loopback URI. Access tokens are process-local, expire after one hour and are bound to the service's resource URL.

JSON-RPC notifications return empty HTTP 202. GET `/mcp` returns 405 because SSE is not implemented. Origin and Host are checked; forwarded headers cannot choose the OAuth issuer. Query bodies, registration counts and concurrent queries are bounded.

Public exposure is a separate **Expose through ngrok** checkbox. Install/configure ngrok independently; `NGROK_BIN` can select its executable. FerriteS100 owns only the tunnel process it launches and does not kill unrelated ngrok processes. A tunnel endpoint change changes the resource and requires client authorization again. An external service/client compatibility test is not implied by local transport tests.

## Spatial query limits

BBox queries use feature AABBs. Nearby queries use a representative bbox centre and approximate spherical distance, not minimum distance to a feature boundary or navigational geodesy. Split dateline-crossing bbox requests into two boxes. These read-only discovery results do not replace chart portrayal, safety contour evaluation or route calculations.

Primary protocol references: [Streamable HTTP](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports), [authorization](https://modelcontextprotocol.io/specification/2025-06-18/basic/authorization).
