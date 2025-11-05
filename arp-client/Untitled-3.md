重写arp-client\src\handlers\session.rs 通过handle_session处理http请求，解析json RPC协议的数据
通过method进行不同处理，使用SessionManager进行全局管理，
其中"session/new"是创建一个child会话，使用manager.create_sessio 创建child 会话，
"session/prompt"是向会话发送消息，会持续返回的是sse数据，直到发现stopReason数据
  
下面是具体步骤
1、客户端发送创建会话请求

{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "session/new",
  "params": {
    "_meta": {
      "agent_name": "Codex",
    },
    "cwd": "/home/user/project",
    "mcpServers": [
    ]
  }
}
2、SessionManager会创建一个会话，并返回会话ID
服务器返回会话ID，收到该消息即可结束http请求
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "sessionId": "sess_abc123def456"
  }
}

3、客户端根据返回的sessionId发送会话生成提示
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "session/prompt",
  "params": {
    "sessionId": "sess_abc123def456",
    "prompt": [
      {
        "type": "text",
        "text": "Can you analyze this code for potential issues?"
      },
    ]
  }
}

4、服务端根据sessionId 查找SessionManager存储的会话信息，通过get_session_stdin
将jsonrpc 原始数据发送给会话

5、child 会话返回流式数据 会话收到 result.stopReason="end_turn" 结束会话
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "stopReason": "end_turn"
  }
}