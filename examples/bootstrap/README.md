# bootstrap
Two stacks (name one): a cluster with dform installed as its controller, and the workload it runs.
```bash
dform plan bootstrap
dform apply bootstrap   # 3 ticks: cluster; node pools and namespace; controller
dform plan workload
dform apply workload    # one tick; or hand it over and run it as the controller:
dform stack handover workload \
  --to 's3("dform-test", "renfry/workload", {endpoint: "http://127.0.0.1:9000"})'
```
The bucket is MinIO's (`eval "$(../../crates/dform-s3/minio.sh start)"`, and a
bucket `dform-test` in it: dform makes none); `--to 'k8s("dform-system/workload")'`
hands it to a directory standing in for the cluster instead.
Then `dform controller run workload`. Features: `provider_config` from open nulls, pending groups, input relations, handover.
