# S3 Lifecycle Configuration for Archived Folds

Trajectory folds with `status='archived'` should transition to cold storage after 30 days to reduce storage costs.

## Background

Ferrosa stores fold trajectory data in S3-compatible object storage (RustFS in development, any S3-compatible service in production). The `trajectory_folds` table tracks fold status. When a fold is archived, its raw trajectory is available via S3 but should tier to cheaper storage over time.

## Development (RustFS)

RustFS supports S3 lifecycle policies via the MinIO client (`mc`):

```bash
# Configure alias for the dev cluster
mc alias set ferrosa-dev http://localhost:19000 rustfsadmin rustfsadmin

# Create lifecycle rule: transition objects older than 30 days
mc ilm add ferrosa-dev/ferrosa-memory \
  --transition-days 30 \
  --storage-class GLACIER \
  --prefix "archived/"

# Verify the rule
mc ilm ls ferrosa-dev/ferrosa-memory
```

## Production

For production S3-compatible services:

### AWS S3
```bash
aws s3api put-bucket-lifecycle-configuration \
  --bucket ferrosa-memory \
  --lifecycle-configuration '{
    "Rules": [{
      "ID": "archive-folds-30d",
      "Filter": {"Prefix": "archived/"},
      "Status": "Enabled",
      "Transitions": [{
        "Days": 30,
        "StorageClass": "GLACIER"
      }]
    }]
  }'
```

### Ferrosa-ctl (when available)
```bash
ferrosa-ctl s3 lifecycle set \
  --bucket ferrosa-memory \
  --rule archive-30d \
  --transition-days 30 \
  --storage-class GLACIER \
  --prefix "archived/"
```

## Configuration

The archive prefix is configured in `ferrosa-memory.toml`:

```toml
[memory]
archive_after_days = 30
```

When a fold's status transitions to `archived`, the MCP server moves its raw trajectory to the `archived/` prefix in S3.

## Monitoring

Check lifecycle rule execution:
```bash
# List archived objects
mc ls ferrosa-dev/ferrosa-memory/archived/ --recursive

# Check storage class of objects
mc stat ferrosa-dev/ferrosa-memory/archived/<fold-id>
```

## Cost Impact

- **Hot storage (S3 Standard):** ~$0.023/GB/month
- **Glacier Instant Retrieval:** ~$0.004/GB/month (83% savings)
- **Glacier Flexible Retrieval:** ~$0.0036/GB/month (84% savings, 3-5hr access)

For a typical deployment with 10GB of archived fold data, lifecycle tiering saves ~$2/month. The primary benefit is at scale with hundreds of tenants.
