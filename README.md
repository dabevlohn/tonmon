# TonMon

This application connects to the TON network using `tonlib-rs` to provide real-time transaction updates. It allows clients to subscribe to specific addresses and monitors for new transactions. Upon detecting a new transaction, the application gathers the full transaction trace and transmits it to the client via WebSocket. To prevent duplicate data, only a single instance of a transaction trace within a single trace is sent.
