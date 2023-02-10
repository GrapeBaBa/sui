window.SIDEBAR_ITEMS = {"constant":[["DEFAULT_RETRIES",""]],"enum":[["ProcessTransactionResult",""],["ReduceOutput",""]],"fn":[["reconfig_from_genesis","Given an AuthorityAggregator on genesis (epoch 0), catch up to the latest epoch and fill in all past epochs’ committee information. Note: this function assumes >= 2/3 validators on genesis are still serving the network."]],"struct":[["AuthAggMetrics","Prometheus metrics which can be displayed in Grafana, queried and alerted on"],["AuthorityAggregator",""],["AuthorityAggregatorBuilder",""],["LocalTransactionCertifier",""],["NetworkTransactionCertifier",""],["QuorumExecuteCertificateError",""],["QuorumSignTransactionError",""],["TimeoutConfig",""]],"trait":[["TransactionCertifier","This trait provides a method for an authority to get a certificate from the network for a specific transaction. In order to create a certificate, we need to create the network authority aggregator based on the Sui system state committee/network information. This is needed to create a certificate for the advance epoch transaction during reconfiguration. However to make testing easier, we sometimes want to use local authority clients that do not involve full-fledged network Sui nodes (e.g. when we want to abstract out Narwhal). In order to support both network clients and local clients, this trait is defined to hide the difference."]],"type":[["AsyncResult",""]]};