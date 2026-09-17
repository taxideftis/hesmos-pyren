---
title: "Agent Hesmos 설계 명세서"
subtitle: "결정론 코어와 자율 스웜의 통합 설계"
doc_id: "HESMOS-DESIGN-001 · REV A"
date: "2026-09"
platform: "bathos Platform · Agent Hesmos"
stack: "Rust Core + Python AI Layer + CLI"
format: "GitHub-flavored Markdown"
source: "Agent_Hesmos_설계명세서.pdf (25p)"
---

# Agent Hesmos 설계 명세서

> **결정론 코어와 자율 스웜의 통합 설계**
>
> bathos Platform · Agent Hesmos — Rust Core + Python AI Layer + CLI
>
> 문서 ID: HESMOS-DESIGN-001 · REV A · 작성 시점: 2026-09

| 항목 | 값 |
|---|---|
| 문서 ID | HESMOS-DESIGN-001 · REV A |
| 부제 | 결정론 코어와 자율 스웜의 통합 설계 |
| 플랫폼 | bathos Platform · Agent Hesmos |
| 기술 스택 | Rust Core + Python AI Layer + CLI |
| 기준 문서 | Agent_Hesmos_설계명세서.pdf (25p) / .docx |
| 다이어그램 | hesmos_assets/ 폴더의 fig1~fig3 PNG 참조 |

## 목차

- [1. Executive Summary](#1-executive-summary)
- [2. 선행 시스템 분석](#2-선행-시스템-분석)
  - [2.1 Swarm AI: 강점과 한계](#21-swarm-ai-강점과-한계)
  - [2.2 Hermes Agent: 강점과 한계](#22-hermes-agent-강점과-한계)
  - [2.3 외부 레퍼런스 검증: swarms, Strands, Relevance AI](#23-외부-레퍼런스-검증-swarms-strands-relevance-ai)
  - [2.4 bathos 실측 분석: 엔진 코드 검증](#24-bathos-실측-분석-엔진-코드-검증)
  - [2.5 설계 시사점 종합](#25-설계-시사점-종합)
- [3. 설계 목표와 원칙](#3-설계-목표와-원칙)
  - [3.1 제품 정의와 배포 형태](#31-제품-정의와-배포-형태)
  - [3.2 8대 설계 원칙](#32-8대-설계-원칙)
  - [3.3 bathos와의 책임 분계](#33-bathos와의-책임-분계)
- [4. 전체 아키텍처](#4-전체-아키텍처)
  - [4.1 시스템 개요](#41-시스템-개요)
  - [4.2 계층별 책임](#42-계층별-책임)
  - [4.3 데이터 흐름과 상태 소유권](#43-데이터-흐름과-상태-소유권)
- [5. 약점 대응 핵심 메커니즘](#5-약점-대응-핵심-메커니즘)
  - [5.1 매핑 매트릭스](#51-매핑-매트릭스)
  - [5.2 결정론 실행 엔진](#52-결정론-실행-엔진)
  - [5.3 구조화 Handoff Contract](#53-구조화-handoff-contract)
  - [5.4 비용·캐시 불변식](#54-비용캐시-불변식)
  - [5.5 신뢰도 누산과 검증 게이트](#55-신뢰도-누산과-검증-게이트)
  - [5.6 조율 병리 방지](#56-조율-병리-방지)
  - [5.7 이벤트 소싱 관측성](#57-이벤트-소싱-관측성)
  - [5.8 보안: 권한·오염·감사](#58-보안-권한오염감사)
  - [5.9 평가 하네스](#59-평가-하네스)
- [6. 코어 컴포넌트 상세 설계](#6-코어-컴포넌트-상세-설계)
  - [6.1 Rust 크레이트 구조](#61-rust-크레이트-구조)
  - [6.2 핵심 인터페이스](#62-핵심-인터페이스)
  - [6.3 Python 바인딩 (hesmos-py)](#63-python-바인딩-hesmos-py)
  - [6.4 데이터 스키마와 CLI](#64-데이터-스키마와-cli)
- [7. 실행 모델과 워크플로](#7-실행-모델과-워크플로)
  - [7.1 턴 라이프사이클](#71-턴-라이프사이클)
  - [7.2 4대 오케스트레이션 패턴](#72-4대-오케스트레이션-패턴)
  - [7.3 장애 복구와 재개](#73-장애-복구와-재개)
  - [7.4 bathos 플랫폼 제어 흐름](#74-bathos-플랫폼-제어-흐름)
- [8. 개발 로드맵과 리스크](#8-개발-로드맵과-리스크)
  - [8.1 Phase 마일스톤](#81-phase-마일스톤)
  - [8.2 GLM-5.3 flash 작업 카드](#82-glm-53-flash-작업-카드)
  - [8.3 리스크와 트레이드오프](#83-리스크와-트레이드오프)

---

## 1. Executive Summary

본 문서는 Swarm형 멀티 에이전트 플랫폼과 Hermes Agent류 퍼스널 에이전트의 강점을 통합하고, 양쪽의 구조적 약점을 제어 계층으로 봉합한 신규 멀티 에이전트 프레임워크 **Agent Hesmos**의 상세 설계서다. Hesmos는 워크플로 오케스트레이션 플랫폼 **bathos**(v0.4.0 — Claude Code 세션을 17역할×7웨이브로 구동하는 메서드 패키지와 단일 Rust 엔진) 위에서 동작하며, 코어는 Rust로, AI 프레임워크 연동 계층은 Python으로, 사용자 접점은 CLI로 구성한다. 설계의 목적은 문장이 아닌 코드로 옮길 수 있는 수준의 스펙을 제공하여, GLM-5.3 flash와 bathos 환경에서 즉시 개발에 착수할 수 있게 하는 것이다.

Hesmos의 핵심 명제는 하나다. **"유연성은 엣지에서, 결정론은 웨이스트에서"**이다. 에이전트가 무엇을 시도할지(LLM 행동)는 비결정적으로 남겨두되, 언제 시작하고 언제 끝나는지, 무엇을 전달받고 무엇을 증명해야 하는지(오케스트레이션 구조)는 Rust 코어가 결정적으로 통제한다. 이 분할이 비결정성·오류 누적·컨텍스트 단절·관측성 부재라는 Swarm류 플랫폼의 4대 만성 질환을 동시에 겨냥한다. 동시에 Hermes의 검증된 규율인 프롬프트 캐시 불변식, Footprint Ladder, 실행 환경 이식성, 모델 중립성을 제도에 이식해 비용과 확장성의 하한을 끌어올린다.

> **15x** — Anthropic 보고 기준, 멀티 에이전트 리서치 시스템의 일반 채팅 대비 토큰 배수 — Hesmos는 이 비용 곡선을 게이트와 캐시 불변식으로 억제한다

**표 1. 핵심 설계 결정 사항**

| 결정 | 내용과 근거 | 기각된 대안 |
|---|---|---|
| Rust 코어 + Python AI 계층 | 그래프·게이트·트레이스·예산 등 제어 평면은 Rust가 소유. LLM/도구 생태계는 PyO3로 Python에 위임. 타입 안전과 결정론을 컴파일 타임에 확보 | 전면 Python(타입 안전성·결정론 부족), 전면 Rust(생태계 진입 비용 과다) |
| CLI 1급 시민 | hesmos run/trace/replay가 공식 인터페이스. 관측과 재생이 CLI에서 먼저 동작하고 SDK 및 Gateway가 그 뒤를 따름 | SDK 우선, CLI 부가 기능(swarms 방식) |
| 계약 기반 handoff | 에이전트 간 전달을 자유 텍스트가 아닌 스키마 검증된 HandoffContract 객체로 강제 | 문자열 전달(swarms 레거시 경로), 프롬프트 맡김(Strands) |
| 이벤트 소싱 트레이스 | 모든 오케스트레이션 단계를 해시 체인으로 연결된 이벤트 로그에 기록. 리플레이·타임트래블 디버깅 제공 | 스팬만 남기는 OTel 단독 운용(실행 재구성 불가) |
| 캐시 불변식 승격 | 시스템 프롬프트 byte-stable, 세션 중 역할 집합 변경은 기본 deferred + 명시적 opt-in | 런타임 자유 전환(Hermes 2.6이 지적한 유연성 대가 수용) |
| bathos 엔진 상속 | 게이트 판정·감사 체인·상태 SSOT·모델 플랜은 bathos 엔진이 이미 소유(제2장 실측) — Hesmos는 엔진 CLI 표면 5종으로 상속하고 동적 스웜 계층만 추가 | 코어에 게이트·감사 재구현(이중 진실 원천), 프로세스 내 자체 샌드박싱 |
| 평가 하네스 내장 | 시드 고정 회귀, golden trace 대조, judge 브릿지를 제품 1급 기능으로 제공 | 평가를 외부 도구에 전적으로 위임 |
| 옵트인 텔레메트리 | 기본 수집 없음. swarms의 import 즉시 전송 문제를 역전 | 기본 ON + env opt-out |

## 2. 선행 시스템 분석

설계 착수에 앞서 네 출처를 교차 검증했다. 첫째는 요구 분석서로 제공된 Swarm AI와 Hermes Agent의 강점·보완점 목록이고, 둘째는 실제 코드와 공식 문서(kyegomez/swarms 저장소, AWS Strands Agents의 Swarm 패턴, Relevance AI의 개념 아티클)에 대한 직접 검증이다. 셋째는 Hesmos가 상주할 플랫폼 본체(taxideftis/bathos)에 대한 소스 수준 실측이다. 넷째는 이 셋을 놓고 본 설계가 반드시 계승하거나 역전해야 할 규율이다. 이 장의 결론은 제3장의 설계 원칙, 제5장의 약점 대응 메커니즘으로 그대로 이어진다.

### 2.1 Swarm AI: 강점과 한계

Swarm류 오케스트레이션의 실질적 이득은 컨텍스트 분할이다. 단일 에이전트에 도구 40개와 규칙 3,000줄을 주면 instruction dilution과 context rot으로 성능이 저하되지만, 스웜은 각 에이전트에 좁은 도구셋과 좁은 프롬프트만 주고 독립된 컨텍스트 윈도우를 쓴다. 여기에 병렬성(breadth-first 작업의 wall-clock 단축), 적응적 라우팅(런타임에 handoff 대상 등록만으로 확장), 확장의 국소성(팀 소유권 경계와 부합), 전문화에 의한 품질(critic 분리로 self-consistency bias 감소)이 덧붙는다. Anthropic의 orchestrator-worker 사례에서 가장 큰 기여 요인으로 보고된 것은 토큰 예산 분산이었다.

반면 보완점은 구조적이다. 같은 입력이 매번 다른 경로를 만드는 비결정성은 재현·회귀 테스트·감사·비용 예측을 동시에 파괴하며, 규제 산업에서 순수 스웜이 반려되는 1순위 사유다. 에이전트마다 시스템 프롬프트와 도구 정의가 중복되어 토큰이 일반 채팅 대비 약 15배 소모되고, 체인이 늘어날수록 신뢰도가 곱셈으로 깎인다. handoff에서 원래 요구사항이 소실되거나 실패한 접근이 재시도되는 컨텍스트 단절, handoff ping-pong·종료 조건 부재·동시 쓰기 충돌 같은 조율 병리, 50회 handoff 뒤 원인 특정이 불가능한 관측성 부재, prompt injection이 handoff를 타고 전파되는 보안 표면 확대, 자유형 출력과 비결정 경로로 인한 평가 불가능이 뒤따른다. 부분 실패 내성은 설계로 확보해야 하는 속성이지 자동으로 오는 것이 아니다.

### 2.2 Hermes Agent: 강점과 한계

Hermes의 차별점은 학습 루프가 제품의 1급 기능이라는 점이다. 에이전트 주도 메모리 관리, 자율 스킬 생성, FTS5 기반 세션 검색, 사용자 모델링의 4갈래가 하나의 폐루프로 묶여 있고, 스킬 표준을 agentskills.io 개방 규격에 맞춰 벤더 종속을 낮췄다. 두 번째 축은 프롬프트 캐시를 불변식으로 승격한 것이다. 시스템 프롬프트를 대화 수명 동안 byte-stable로 유지하고, 도구·스킬·메모리를 바꾸는 명령은 기본 deferred(다음 세션 반영), --now만 명시적 opt-in으로 허용한다. 장시간 대화의 실제 비용은 모델 단가가 아니라 캐시 적중률이 결정한다는 통찰을 리뷰 기준으로 못 박은 점이 실질적 규율이다.

또한 Footprint Ladder(기존 코드 확장, CLI+스킬, 서비스 게이트 도구, 플러그인, MCP 서버, 새 코어 도구의 순서로 가장 가벼운 단에서 해결), 7종 터미널 백엔드와 serverless persistence로 대표되는 실행 환경 이식성, 25+ 메신저 접점, 18+ 프로바이더 런타임 리졸버, 39,000개 테스트로 상징되는 엔지니어링 규율, ShareGPT 트래젝토리 생성 같은 연구 지향성이 강점이다. 반면 보안 경계가 OS뿐이라는 자기 선언, import 시점에 임의 Python을 실행하는 스킬(공급망 위험), 메모리·스킬이 세션을 넘어 살아남는 자기개선 루프의 부작용(재현 불가, 오염 영속화, 스킬 드리프트), delegate_tool.py 수준에 머무는 얕은 멀티 에이전트 조율(상태 머신·게이트·인계 계약·종료 조건 부재), 정책적으로 배제된 관측성, 캐시 불변식의 대가로서의 동적 전환 불가, RBAC·감사·비용 귀속이 없는 단일 테넌트 설계, 넓은 공격·유지보수 표면, 빠른 변화 속도, 동기 에이전트 루프가 보완점으로 지적된다. Hesmos는 이 목록 중 멀티 에이전트 조율·관측성·감사·다중 테넌시를 제품 안으로 수렴시킨다.

### 2.3 외부 레퍼런스 검증: swarms, Strands, Relevance AI

**kyegomez/swarms**(별 7,172, Python, Apache-2.0, PyPI 15.0.2)는 실제 소스 검증에서 흥미로운 이중성을 보였다. 구조적으로는 단일 프리미티브 조합 모델이 우수하다. Agent 하나가 14개 이상 구조체(SequentialWorkflow, ConcurrentWorkflow, AgentRearrange, SwarmRouter, GraphWorkflow, MixtureOfAgents, MajorityVoting, GroupChat 등)에서 재사용되고, SwarmRouter가 14종 SwarmType을 O(1) 팩토리로 교체하며 fallback_swarms 체인을 제공한다. 최신 코드는 공유 Conversation을 문자열 이어붙이기가 아닌 typed-turns(메시지 배열)로 다음 에이전트에 전달하는 방향으로 진화했고, einsum 스타일 flow DSL "a -> b, c", LiteLLM 경유 100+ 프로바이더, MCP 양방향(클라이언트 + MCPDeployer 서버화), OTel 트레이싱 내장, 타입화된 에러 계층(AgentToolExecutionError는 재시도 무의미로 분류)이 확인됐다.

반면 코드 관찰 기반 한계도 명확하다. Agent 클래스가 4,481행·100+ 파라미터의 god-class이고, BaseSwarm 계약이 존재하지 않아 "run()이 있다"는 덕 타이핑만 남는다. AgentType = Union[Agent, Callable, Any] 사실상 Any, llm: Any 등 타입 안전성 부재, import swarms 즉시 텔레메트리 전송(기본 ON, env로만 opt-out), GraphWorkflow의 networkx/rustworkx 이중 그래프 엔진에도 불구하고 temperature·시드 고정 옵션 없음, drift 판정과 consensus가 LLM 판정이라 동일 입력 재현 불가, ConcurrentWorkflow의 실패가 "(failed)" 문자열로만 남는 에러 삼킴, 스레드풀과 asyncio의 혼용으로 취소·백프레셔 시맨틱 부재가 관찰됐다. 문서의 RAG·long_term_memory는 구현이 빠진 채 파라미터만 잔존한다.

**AWS Strands Agents의 Swarm**은 "개발자는 에이전트 풀만 제공하고, 경로는 에이전트가 결정한다"는 분산 라우팅 모델이다. 실행 시 모든 노드에 handoff_to_agent(agent_name, message, context) 툴이 자동 주입되고, 툴 미호출 = 완료(COMPLETED)라는 자연스러운 종료 판단, SharedContext의 JSON 직렬화 강제, LLM에 노출되지 않는 invocation_state 이중 채널, max_handoffs=20/max_iterations=20/execution_timeout=900s/node_timeout=300s의 4중 안전장치, 반복 handoff 감지 윈도우, at-least-once 재개 명세와 멱등성 요구, uncommitted turn rollback, OTel 스팬 + MultiAgentHandoffEvent 같은 타입화 이벤트가 설계에 참고할 만하다. 한계도 문서가 스스로 인정한다. "The path is emergent" — 회귀 테스트와 감사에 불리하고, handoff마다 원태스크·노드 히스토리·SharedContext 전체가 재주입되어 컨텍스트가 선형적으로 폭증하며, 한계 도달은 성공이 아니라 FAILED 하나로 뭉개진다. Python(가변 공유 상태)과 TS(직렬화 전달)의 의미론 불일치도 이식성을 깎는다.

**Relevance AI의 아티클**은 구현 실체가 없는 개념 문서지만 두 가지 실무적 차별점이 있다. 비용 통제(Cost Control)를 엔터프라이즈 과제로 명문화한 점과 모니터링·로깅을 설계 원칙으로 격상시킨 점이다. Swarm Controller / Communication Layer / Resource Manager의 3분할은 Hesmos의 오케스트레이션 엔진·메시지 버스·예산 관리자 매핑으로 그대로 이식됐다. 반면 Collaborative Learning을 현재 가능한 기능처럼 서술하는 과장, 권한·PII·감사 논의 전무라는 거버넌스 공백은 Hesmos가 채워야 할 반면교사다. 기술 명세는 Strands의 파라미터 표와 안전장치 문서화 수준을, 비용과 거버넌스는 이 아티클이 제기한 과제 수준을 기준선으로 삼는다.

### 2.4 bathos 실측 분석: 엔진 코드 검증

**bathos**(v0.4.0, MIT)는 Hesmos가 상주할 실행 기반이므로 출처가 아니라 검증 대상으로 직접 분석했다. bathos는 Claude Code 세션을 "17 전문 역할 × 7웨이브 파이프라인 × Scale-Adaptive Lv0~4"로 구동하는 메서드 패키지이며, 두 평면으로 나뉜다. 인간이 편집하는 오케스트레이션 평면(.claude/ 아래 슬래시 커맨드 34종, 역할 정의 17종, 안전 훅 6종)과 신뢰가 요구되는 모든 것을 소유하는 엔진 평면(단일 정적 Rust 바이너리)이다. 엔진 실측 결과는 10 크레이트·26,943 LOC·629 테스트(훅 결정론 테스트 86건 별도)로, 상태 SSOT(manifest.json — JSON Schema 검증·원자적 쓰기), 변조방지 감사 sha256 해시 체인(단일 writer 직렬화, E-AUDIT-TAMPER), PASS/CONCERNS/FAIL 3값 게이트(critical>0→FAIL 강제, facilitator 빈 값 불가), 결정론 라우팅 산술(권고/확정 분리), 스토리 무결성 D1/D2/D3(6 필수 섹션·[Source:] 추적·sha256 staleness)이 코드로 확인됐다. 게이트 FAIL은 exit 2를 반환하는 gate-enforce 훅이 다음 웨이브 진입을 물리 차단한다.

모델 계층도 검증했다. bathos는 model-plan.json으로 역할·웨이브 단위 런타임을 지정하며(Claude/Glm/Codex/Kimi/DeepSeek/Qwen 6종), resolve_effective 5단계 우위 체인과 Runtime::is_env_global() 술어로 혼합 규칙을 일반화했다. GLM·Kimi·DeepSeek·Qwen은 프로세스 전역 ANTHROPIC_BASE_URL 스왑 방식이라 한 세션에서 역할별 백엔드 분리가 물리적으로 불가능하고, 이를 위반하면 bathos model validate가 E-MODEL-MIX(exit 2)로 차단한다. GLM 백엔드 전환은 환경변수 2개(api.z.ai Anthropic 호환 엔드포인트)로 완전 가역이며, 스모크 테스트로 라이브 실증이 봉인돼 있다. 설계 시사점은 명확하다. 게이트·감사·상태·모델 플랜이라는 결정론 자산을 Hesmos 코어에 재발명하는 것은 이중화일 뿐이므로, Hesmos는 bathos 엔진이 이미 소유한 규율을 CLI 표면으로 상속하고, bathos가 갖지 않은 동적 스웜 계층(노드 그래프·노드 간 계약·타입 봉투·예산 봉투·평가 하네스)을 그 위에 얹는다. 이것이 제3장 표 4의 개정된 분계다.

### 2.5 설계 시사점 종합

네 출처의 교차 검증은 Hesmos가 계승할 것과 역전할 것을 분명히 나눈다. 계승 목록은 검증된 패턴의 모음이고, 역전 목록은 위 표의 결정 사항이 이미 다루었다. bathos 행은 "출처가 아니라 기반"이라는 지위를 반영해, 역전 칸에 상속 방식을 적는다. 아래 표가 이 설계서의 방향타다.

**표 2. 계승·역전 매트릭스**

| 계승 (adopt) | 출처 | 역전 (invert) |
|---|---|---|
| 단일 프리미티브 + 조합 구조체 | swarms | 덕 타이핑 계약 → Rust trait으로 고정 |
| flow DSL "a -> b, c" | swarms AgentRearrange | DSL 파싱을 Rust로, 오류를 컴파일 타임에 |
| typed-turns 메시지 전달 | swarms 최신 경로 | role/content dict → 타입 부착 Envelope로 승격 |
| 모델·스웜 이중 폴백, 타입화 에러 | swarms | 에러 삼킴 → reason code 필수화 |
| 종료=툴 미호출, 4중 안전장치 | Strands Swarm | 한계 도달=FAILED 뭉개기 → 구조화 reason code |
| SharedContext JSON 강제, 비노출 채널 | Strands | 가변 공유 dict → 불변 스냅샷 전달 |
| 체크포인트+멱등성 재개, 이벤트 태스노미 | Strands | at-least-once를 그대로 두되, 게이트로 1회 성공률 상향 |
| 캐시 불변식, Footprint Ladder | Hermes | 단일 테넌트 → 조직 계층(팀·예산·감사) 내장 |
| 옵트인 텔레메트리 원칙 | Hermes | swarms의 import 즉시 전송 → 부작용 제로 |
| 비용 통제 1급 과제화 | Relevance AI | 사용량 집계에 그침 → 사전 차단형 Budget Manager |
| 감사 해시 체인 audit append/verify | bathos | 스웜 로그 별도 체인 → trace seal로 bathos 체인에 합류 |
| 게이트 3값 어휘·exit 2 물리 차단 | bathos | 무판정 진행 → 웨이브 게이트도 동일 어휘·관례 준수 |
| 스토리 D1/D2/D3 제로 컨텍스트 로스 | bathos | 웨이브 간 계약 → 노드 간 HandoffContract로 일반화 |
| 결정론 라우팅 산술·권고/확정 분리 | bathos | 거시 Lv0~4는 bathos, 미시 노드 선택은 라우터 노드+기록 |

## 3. 설계 목표와 원칙

### 3.1 제품 정의와 배포 형태

Hesmos는 두 축으로 배포된다. 첫째, **코어 라이브러리**(Rust 크레이트 hesmos-core + Python 패키지 hesmos-py)는 사용자 서비스에 임베딩되는 멀티 에이전트 엔진이다. 둘째, **게이트웨이 번들**은 옵션 패키지로 HTTP/WebSocket 접점, 팀 단위 예산·감사 대시보드 백엔드를 제공한다. 어느 쪽이든 최초이자 공식적인 인터페이스는 CLI다. "hesmos run plan.hes"로 실행하고, "hesmos trace replay <id>"로 재현하는 경험이 SDK보다 먼저 설계된다. 관측과 재생이 CLI에서 먼저 동작하고 SDK 및 Gateway가 그 뒤를 따른다. 이는 swarms CLI가 편의 기능 수준에 머문 것과 의도적으로 차별화되는 포지셔닝이며, "Rust 코어와 CLI"라는 배포 단위의 자연스러운 귀결이기도 하다.

1차 목표 사용자는 개발자와 소규모 팀이다. 개인 워크스테이션에서 bathos 로컬 런타임으로 상주시키거나, CI·야간 배치에서 무인 실행시키는 형태를 기본 시나리오로 삼는다. 조직 계층(RBAC, 중앙 정책, 비용 귀속)은 게이트웨이 번들이 책임지되, 코어 데이터 모델에 team_id·actor·reason code가 처음부터 심겨 있어 후살이 불필요하다.

### 3.2 8대 설계 원칙

원칙은 검토 가능한 문장으로 쓴다. 각 원칙은 코드 리뷰에서 거부 사유가 되며, 제5장의 메커니즘과 제6장의 인터페이스가 이 원칙을 위반하지 않는지 검증된다.

**표 3. 설계 원칙과 위반 판정 기준**

| 원칙 | 정의 | 위반 판정 예 |
|---|---|---|
| P1 결정론 웨이스트 | 그래프 해석·스케줄·라우팅·상태 전이는 Rust가 소유하고 시드 고정 시 재현 가능해야 한다 | LLM 출력이 다음 노드 선택을 바꾸는 코드 |
| P2 타입 안전 경계 | 크레이트 간·FFI 경계의 모든 데이터는 serde/pydantic 검증을 통과한다 | Any/Any로 흘려보내는 파라미터 추가 |
| P3 계약 우선 handoff | 에이전트 간 전달은 스키마 검증된 HandoffContract 없이 불가능하다 | 프롬프트 문자열로 전달 우회 |
| P4 캐시는 불변식 | 시스템 프롬프트·도구 정의는 턴 내 byte-stable. 변경은 deferred가 기본 | 세션 중 슬라이시 명령의 --now 기본화 |
| P5 Footprint Ladder | 새 기능은 기존 코드 확장 > CLI+스킬 > 게이트 도구 > 플러그인 > MCP > 코어 도구 순서로 해결 | 간단 기능의 코어 도구 신규 추가 |
| P6 이벤트 소싱 | 오케스트레이션 상태 변화는 전부 append-only 이벤트. 파생 뷰는 재구성 가능해야 한다 | 메모리에서만 유지되는 상태 |
| P7 최소 권한·옵트인 | 에이전트별 권한 프로파일 필수, 텔레메트리·외부 전송은 기본 OFF | 기본 ON 수집, denylist 셸 허용 |
| P8 bathos 엔진 상속 | 게이트 기록·감사 체인·상태 SSOT·모델 플랜은 bathos 엔진 CLI로 위임하고, Hesmos는 동적 스웜 계층만 소유한다 | 코어에 감사 체인·게이트 기록 재구현(이중 진실 원천) |

### 3.3 bathos와의 책임 분계

bathos는 결정론 자산을 코드로 소유한 워크플로 플랫폼이고, Hesmos는 그 위에 얹히는 동적 스웜 계층이다. 제2장 실측에서 확인했듯 bathos 엔진은 게이트 판정·감사 체인·상태 SSOT·모델 플랜을 이미 구현하고 629개 테스트로 봉인하고 있으므로, Hesmos 코어가 이를 재구현하면 두 개의 진실 원천이 생겨 감사가 오히려 약해진다. 아래 표는 bathos v0.4.0 실측 기준으로 개정한 소유권의 최종 분배이며, 이 분배를 어기는 기능은 설계 단계에서 반려된다. Hesmos가 bathos 엔진에 요구하는 것은 CLI 표면 5종(state init/validate, gate verdict/show, audit append/verify, model validate, wave activate/show)과 오케스트레이션 평면의 훅 바인딩뿐이고, bathos가 Hesmos에 요구하는 것은 스웜 이벤트의 trace seal 제출과 웨이브 게이트 어휘(PASS/CONCERNS/FAIL) 준수뿐이다.

**표 4. bathos / Hesmos 소유권 분계 (bathos v0.4.0 실측 기준)**

| 관심사 | bathos 소유 (엔진 평면) | Hesmos 소유 (스웜 계층) |
|---|---|---|
| 워크플로 골격 | 7웨이브 상태 전이, 동시성 ≤3(E-CONCURRENCY), Scale-Adaptive Lv0~4 라우팅(권고/확정 분리) | 노드 그래프 컴파일·위상 웨이브, 노드 간 적응 라우터 노드 |
| 게이트 | PASS/CONCERNS/FAIL 어휘, 판정 기록, gate-enforce 훅의 exit 2 물리 차단 | pre/post 정책셋·루브릭·신뢰도 누산 판정 수행, 결과를 bathos 어휘로 기록 |
| 감사 | sha256 해시 체인 저장소, audit append/verify(E-AUDIT-TAMPER) | 스웜 이벤트 택소노미 9종, trace seal을 bathos 체인에 봉인 |
| 상태 | manifest.json SSOT, JSON Schema 검증·원자적 쓰기 | Envelope·HandoffContract·세션 스키마, WAL 커밋 단위 정의 |
| 컨텍스트 무손실 | 스토리 파일 D1/D2/D3, E-STALE 재컴파일 강제 | 노드 간 HandoffContract, 불변 스냅샷 슬라이싱, 8K 전달 상한 |
| 모델 | model-plan.json, Runtime 6종 분류, E-MODEL-MIX 검증 | 프로바이더 어댑터·캐시 불변식·예산 봉투 집행 |
| 안전 | careful-guard·freeze-guard 훅(파괴 명령·소유 경계 외 편집 차단) | 권한 프로파일·taint 마킹·스킬 서명 검증 |

## 4. 전체 아키텍처

### 4.1 시스템 개요

그림 1은 Hesmos의 4계층 구조를 보여준다. 접점 계층(CLI·Gateway·Python SDK·ACP 에디터)은 모두 동일한 코어 계약을 통해서만 코어에 접근한다. 코어(Rust)는 그래프 엔진, 핸드오프 라우터, 가드 레이어, 예산 관리자, 트레이스 스토어, 세션 스토어의 6개 컴포넌트로 구성되며, 이 중 결정론이 요구되는 3개(그래프 엔진·핸드오프 라우터·가드 레이어)는 시드 기반 재현을 보장한다. Python AI 계층은 PyO3 FFI를 통해서만 코어에 연결되며 프로바이더 어댑터, 도구 실행기, 메모리·스킬, 평가 하네스를 담당한다. 최하위 bathos는 두 평면을 제공한다 — 오케스트레이션 평면(역할·커맨드·안전 훅 6종)과 엔진 평면(state·router·wave·gate·story·plug 크레이트, 제2장 실측)이며, Hesmos 코어는 엔진 CLI 표면 5종으로만 결합한다(표 4). 화살표는 인접 계층 간 허용 흐름만 표현한다 — 계층 건너뛰기는 P8 위반이다.

![그림 1. Agent Hesmos 시스템 아키텍처](./hesmos_assets/fig1_architecture.png)

**그림 1. Agent Hesmos 시스템 아키텍처**

이 구조의 요점은 "AI는 FFI 너머에 격리된 서비스 제공자"라는 관점이다. LLM 호출, 도구 실행, 메모리 접근은 모두 Python 계층의 API 호출이고, 코어는 호출 전후에 게이트를 심는다. swarms가 Agent 4,481행 안에 LLM 호출·툴 파싱·메모리·캐싱·마켓플레이스를 함께 둔 것과 정반대로, Hesmos의 Agent는 상태와 권한의 명세일 뿐 실행 로직을 갖지 않는다. 실행 로직은 그래프 엔진의 위상 정렬 웨이브(topological waves)가 운영한다.

### 4.2 계층별 책임

**접점 계층**은 상태를 갖지 않는다. CLI는 세션 파일 경로와 플래그만 해석해 코어 함수를 호출하고, Gateway는 HTTP/WebSocket 요청을 코어의 SessionHandle로 매핑한다. **코어**의 6개 컴포넌트 중 그래프 엔진은 계획(plan)을 DAG로 컴파일하고 위상 정렬로 웨이브를 나눠 실행 가능한 단위로 배포한다. 핸드오프 라우터는 HandoffContract의 done-criteria와 역할 매칭으로 다음 노드를 결정하고, 루프 가드(최대 handoff 수, ping-pong 감지 윈도우)를 함께 집행한다. 가드 레이어는 pre-gate(권한·예산·스키마)와 post-gate(출력 스키마·루브릭·신뢰도)로 나뉘며, 모든 판정은 이벤트로 기록된다. 예산 관리자는 세션·팀·에이전트 3단위로 토큰/비용 상한을 유지하고, 트레이스 스토어는 해시 체인 이벤트 로그, 세션 스토어는 SQLite WAL 기반 체크포인트를 담당한다.

**Python AI 계층**은 hesmos-ffi(pyO3 확장)로 노출된 불변 API만 사용한다. 프로바이더 어댑터는 GLM-5.3 flash를 1차 모델로 두고 18+ 프로바이더·3가지 API 모드(chat completions / responses / anthropic)를 런타임 리졸버로 흡수한다. 도구 실행기는 MCP 클라이언트를 포함하되, 모든 도구는 권한 프로파일을 사전 요구한다. 메모리·스킬은 Hermes의 agentskills.io 규격을 수용하되, 로딩 시점 코드 실행 대신 선언적 스킬 매니페스트를 기본으로 두고 임의 실행은 명시적 서명된 패키지만 허용한다. 모델 선택은 bathos model-plan.json(런타임 glm)과 정합되어, 세션 전체가 단일 env-global 백엔드를 공유하는 bathos의 혼합 규칙을 따른다. **bathos 엔진**과의 접점은 제3장 표 4의 분계를 따른다.

### 4.3 데이터 흐름과 상태 소유권

한 턴의 데이터는 정확히 한 방향으로 흐른다. 접점이 Task를 받으면 코어가 Plan(노드·간선·게이트·예산 봉투)을 컴파일하고, 각 웨이브에서 노드 단위로 Envelope(제6장 스키마)가 생성되어 Python 계층으로 전달된다. Python 계층은 LLM/도구 결과를 Envelope에 채워 돌려주고, 코어가 post-gate를 통과시킨 뒤에만 WAL에 커밋한다. 상태의 소유권은 단일하다 — 실행 중 상태는 코어가, 대화 내용은 세션 스토어가, 무엇이 언제 왜 일어났는지는 트레이스 스토어가 유일한 진실 원천이다. 에이전트는 자기 컨텍스트 윈도우 안에서만 유효한 로컬 뷰를 가질 뿐, 전역 상태를 직접 수정할 수 없다. 이 단일 소유권이 동시 쓰기 충돌을 구조적으로 제거한다.

## 5. 약점 대응 핵심 메커니즘

### 5.1 매핑 매트릭스

아래 표는 Swarm/Hermes에서 확인된 8대 약점을 Hesmos의 대응 메커니즘과 검증 기준에 1:1로 매핑한다. 약점 하나가 최소 하나의 검증 가능한 기준을 갖는다는 점이 이 매트릭스의 요구사항이다. 기준이 없는 대응은 구현이 아니라 소원이기 때문이다. 전 영역 균형을 원칙으로 하되, 각 기준은 제8장 로드맵의 해당 Phase에 배정되어 납기와 함께 추적된다.

**표 5. 약점 → 대응 메커니즘 매핑 매트릭스**

| 약점 | 원인(출처) | Hesmos 대응 | 검증 기준 |
|---|---|---|---|
| 비결정성·재현 불가 | LLM 라우팅이 경로 결정(Swarm 2.1, Strands "path is emergent") | 결정론 실행 엔진: 그래프 컴파일+시드+게이트 (5.2) | 동일 입력+시드로 trace 해시 일치 |
| 토큰 비용 15x | 시스템 프롬프트·도구 정의 중복 전달(Swarm 2.2) | 캐시 불변식+Footprint Ladder (5.4) | 캐시 적중률 지표, 턴당 중복 토큰 상한 |
| 오류 누적(곱셈 신뢰도) | 게이트 없는 체인(Swarm 2.3) | 신뢰도 누산+post-gate 보정 (5.5) | 게이트 통과률, 체인별 신뢰도 리포트 |
| 컨텍스트 단절 | 프롬프트에 맡긴 전달(Swarm 2.4, Hermes 2.4) | HandoffContract 필수화 (5.3) | 계약 스키마 통과율 100% 강제 |
| 조율 병리 | 종료 조건 부재·쓰기 충돌(Swarm 2.5) | 종료 선언자·루프 가드·단일 소유권 (5.6) | 핑퐁 감지 적중, 무한 루프 0건 |
| 관측성 부재 | 스팬만 남고 실행 재구성 불가(Swarm 2.6, Hermes 2.5) | 이벤트 소싱+해시 체인+리플레이 (5.7) | 어느 단계든 이벤트 시간 여행 가능 |
| 보안 표면 확대 | injection 전파·권한 합성(Swarm 2.7, Hermes 2.1-2.2) | 권한 프로파일+taint 격리+감사 체인 (5.8) | 오염 전파 0건, 모든 파괴적 작업 서명 존재 |
| 평가 불가 | 자유형 출력·비결정 경로(Swarm 2.8) | golden trace 대조+시드 회귀+judge 브릿지 (5.9) | 회귀 스위트 통과를 CI 게이트로 |

### 5.2 결정론 실행 엔진

그래프 엔진은 계획을 DAG로 컴파일한다. 노드는 실행 단위(Stage), 간선은 데이터 의존, 게이트는 노드 경계에 매달린 판정 포인트다. 실행은 위상 정렬 웨이브 단위로 진행되며, 같은 웨이브의 노드는 병렬 실행돼도 커밋 순서는 시드 기반 PRNG와 결정적 정렬 키로 고정된다. LLM이 다음 노드를 "선택"하는 것은 차단되고, LLM은 현재 노드의 목표 달성만 책진다. 경로가 런타임에 필요한 경우(적응적 라우팅)에는 라우터 노드를 명시적으로 그래프에 추가해야 하며, 라우터의 판정 입력과 출력은 모두 이벤트로 기록된다. 이는 Strands식 "경로는 창발한다"를 정면으로 역전하는 선택이지만, 적응성을 없애는 것이 아니라 적응성의 발생 지점을 관측 가능하게 만드는 것이다.

재현 프로토콜은 단순하다. 세션 id, 계획 해시, PRNG 시드, 모델 응답 캐시(선택)의 4요소가 주어지면 코어는 동일한 노드 순서·동일한 게이트 판정·동일한 커밋 순서로 재생한다. 모델 응답이 캐시되면 바이트 단위 재현이고, 캐시되지 않으면 구조(경로·게이트 판정) 단위 재현이 보장된다. swarms에서 drift 판정과 consensus가 매번 달랐던 것과 대비되는 지점이며, 회귀 테스트의 전제 조건을 처음부터 성립시킨다.

### 5.3 구조화 Handoff Contract

handoff는 계약 객체다. 계약이 스키마 검증을 통과하지 못하면 다음 에이전트는 시작되지 않고, 실패한 계약은 reason code와 함께 이전 에이전트로 되돌아가거나 세션이 중단된다. 계약 필드는 아래와 같으며, original goal은 모든 계약에 원문으로 주입되어 3번째 handoff에서 요구사항이 소실되는 고전적 실패를 차단한다. failed_approaches는 이미 시도해 실패한 접근의 목록으로, 다음 에이전트의 재시도 낭비를 막는다. assumptions는 잠정 가정을 확정 사실로 굳어 전파하는 문제를 막기 위해 상태(flag)를 강제한다.

**표 6. HandoffContract 스키마**

| 필드 | 내용 | 부재 시 판정 |
|---|---|---|
| goal_original | 최초 사용자 요구 원문(불변) | 컴파일 오류 — 계약 생성 불가 |
| goal_current | 이 단계의 구체 목표 | 게이트 거부(reject) |
| invariants | 보존해야 할 제약(금지 사항 포함) | 게이트 거부 |
| done_criteria | 완료 판정 기준(기계 판독 가능) | 게이트 거부 — 재시도 없음 |
| artifacts | 산출물 참조(경로+해시) | post-gate 실패로 처리 |
| failed_approaches | 실패한 접근+사유 | 경고와 함께 진행(권장 필드) |
| assumptions | 잠정 가정+확정 여부 플래그 | 게이트 거부 |
| confidence | 생성자 자기평가 0.0~1.0 | 임계치 미달 시 bounded retry |

계약의 전달은 전체 트랜스크립트 복사가 아니라 **불변 스냅샷 + 필요 슬라이스**다. 수신 에이전트는 goal_original, 직전 계약 요약, 자기 노드 입력, SharedKnowledge(공유 컨텍스트 중 자기 권한으로 읽을 수 있는 키)만 받는다. Strands가 handoff마다 히스토리 전체와 SharedContext 전체를 재주입해 컨텍스트가 선형 폭증한 것과 달리, Hesmos는 전달 크기 상한(기본 8K 토큰)을 게이트 G0에서 검사한다. 히스토리가 필요한 노드는 요약 노드를 거쳐 요약본을 받는 구조로, 컨텍스트 관리가 프롬프트의 몫이 아니라 그래프의 몫이 된다.

### 5.4 비용·캐시 불변식

Hermes가 리뷰 기준으로 못 박은 캐시 규율을 Hesmos는 코어 불변식으로 승격한다. 시스템 프롬프트는 세션 수명 동안 byte-stable이며, 코어는 턴마다 프롬프트 해시를 검증해 위반 시 세션을 중단한다. 도구 정의·스킬·메모리를 바꾸는 명령은 기본 deferred(다음 세션 반영)이고 --now는 명시적 opt-in이다. 유일한 예외는 context compression이며, 이 역시 이벤트로 기록된다. 캐시 breakpoint 구성(stable/context/volatile 3계층)은 프롬프트 빌더가 자동 배치하므로 사용자 코드가 캐시 경계를 깨뜨릴 여지를 남기지 않는다.

Footprint Ladder는 Hermes의 규율을 그대로 채택한다. 새 기능은 기존 코드 확장, CLI+스킬, 서비스 게이트 도구, 플러그인, MCP 서버, 새 코어 도구 순서에서 가장 가벼운 단으로 해결해야 한다. 모든 코어 도구는 매 API 호출마다 전송되므로 코어 도구 추가는 전 사용자가 영구 부담하는 비용이라는 Hermes의 근거는 동일하게 유효하다. 여기에 Hesmos만의 추가 장치로, 토큰 예산 봉투(budget envelope)가 세션 개시 시점에 고정되고, 예산 80% 도달 시 경고 이벤트, 100% 도달 시 suspend+checkpoint가 발생한다. 작업의 가치가 토큰 비용을 넘지 못하면 스웜이 경제적으로 틀린 선택이라는 지적(15x 배수)에 대한 시스템적 답변은 "무조건 차단"이 아니라 **가시화 + 상한 + 저비용 경로(Footprint Ladder) 우선**의 3단 방어다.

### 5.5 신뢰도 누산과 검증 게이트

에이전트 수를 늘리면 신뢰도가 곱셈으로 깎인다는 사실은 수학적 제약이지, 게이트로 보정하면 완화된다. Hesmos는 각 노드 출력에 confidence(자기평가)와 gate_score(기계 검증)를 두고, 체인의 유효 신뢰도는 min(gate_score)에 수렴하도록 설계한다 — 즉 가장 약한 링크가 체인 품질을 결정하며, post-gate는 그 약한 링크를 조기에 발견한다. post-gate는 출력 스키마 검증, done_criteria 대조, 루브릭 체크(규칙 기반 우선, 필요 시 LLM judge)로 구성되고, 실패 시 bounded retry(기본 2회) 후 상위 노드로 에스컬레이션한다. swarms가 개별 실패를 "(failed)" 문자열로 묻어두는 것과 달리, Hesmos의 모든 게이트 판정은 reason code를 필수로 갖는 이벤트다.

### 5.6 조율 병리 방지

핑퐁·종료 조건 부재·쓰기 충돌·과잉 위임의 4대 병리에 각각 하나의 장치를 둔다. **종료 선언자**는 그래프의 terminal 노드로 고정되어 "충분히 했다"를 에이전트 임의로 선언할 수 없다. **루프 가드**는 최대 handoff 수(기본 20, Strands의 기본값 채택)와 ping-pong 감지(A→B→A 패턴, 관측 윈도우 기본 8 스텝)를 집행해 위반 시 halt_with_reason을 발생시킨다. **쓰기 충돌**은 상태 소유권 단일화(제4장)로 구조적으로 제거되고, 외부 자원(파일·레코드)에 대해서는 노드 단위 리스(lease)를 게이트가 발급한다. **과잉 위임**은 과잉 위임 게이트(예상 토큰이 임계 미달인 서브태스크의 서브에이전트 스폰 차단)로 막는다. 이 네 장치는 모두 코어의 핸드오프 라우터·가드 레이어가 소유하므로, 사용자가 프롬프트에서 우회할 수 없다.

### 5.7 이벤트 소싱 관측성

관측성은 부가 기능이 아니라 상태 관리의 부산물이다. 코어의 모든 상태 변화는 append-only 이벤트로 기록되며, 이벤트 로그가 유일한 진실 원천이다. 각 이벤트는 직전 이벤트의 해시를 포함하는 체인으로 연결되어, 사후 변조가 감지 가능한 감사 증거가 된다. 10개 에이전트 50회 handoff 뒤 어느 단계가 원인인지의 질문에 대해, Hesmos는 "hesmos trace replay <session_id> --at step 37"이라는 답을 제공한다. 택소노미는 Strands의 node_start/handoff/node_stop/result를 확장한 9종(표 7)이며, OTel로의 브릿지는 opt-in 싱크(bathos 소유)로 내보내는 얇은 어댑터일 뿐이다. 세션 봉인(trace.seal)의 최종 해시는 bathos audit append로 공개 체인에 기록되어, 스웜 관측과 플랫폼 감사가 단일 해시 체인에서 만난다 — 이중 체인 운영은 P8 위반이다.

**표 7. 코어 이벤트 택소노미**

| 이벤트 | 의미 | 주요 속성 |
|---|---|---|
| session.open / close | 세션 개시·종료(예산 봉투 포함) | session_id, seed, budget |
| plan.compiled | 계획의 DAG 컴파일 완료 | plan_hash, node_count |
| gate.pass / gate.fail | pre/post 게이트 판정 | gate_id, reason_code, score, judge.*(옵션 — judge 브릿지 사용 시: prompt_hash·temperature·model_version·verdict. TYPE-3 attrs_optional — 택소노미 9종 유지, W3-5 해소) |
| node.start / node.stop | 스테이지 실행 시작·종료 | node_id, agent, wave |
| handoff.request / accept | 계약 제출·수락 | contract_hash, from, to |
| llm.call / tool.call | 외부 호출(usage 계측 포함) | provider, tokens_in/out, latency |
| budget.event | 예산 경고·초과 | level, spent, remaining |
| trace.seal | 세션 봉인(최종 해시) | chain_head_hash |

### 5.8 보안: 권한·오염·감사

Hermes의 자기 선언 — "적대적 LLM에 대한 유일한 보안 경계는 OS다" — 를 Hesmos는 설계 전제로 수용한다. 따라서 프로세스 내 방어(승인 게이트, 패턴 스캐너)를 경계로 포장하지 않고, bathos 훅(careful-guard·freeze-guard)과 Claude Code 프로세스 경계를 1차 경계로 두고 코어는 그 안에서 **권한·오염·감사**의 3축을 집행한다. 권한 축: 모든 에이전트는 권한 프로파일(툴 allowlist, 자원 상한, 네트워크 범위)을 필수로 가지며, 체인 전체의 유효 권한이 합집합이 되는 confused deputy를 막기 위해 계약에 노드별 권한 상한을 명시한다. 오염 축: 외부 문서를 읽은 결과는 taint 마킹되고, taint된 데이터는 메모리·스킬 영속화 대상에서 제외된다 — 학습 루프가 공격 지속성 메커니즘이 되는 것을 구조적으로 차단한다. 감사 축: 파괴적 작업은 실행 주체(actor), 근거(contract 해시), 판정(gate 기록)이 해시 체인에 남는다. 스킬은 매니페스트 선언형이 기본이고 임의 코드 실행은 서명된 패키지로 한정해 npm식 공급망 위험의 재현을 막는다.

### 5.9 평가 하네스

평가를 외부 도구의 몫으로 두면 회귀가 침묵 속에 누적된다. Hesmos는 평가 하네스를 제품 1급 기능으로 내장한다. 첫째, **golden trace 대조** — 승인된 세션의 경로·게이트 판정을 골든 샘플로 저장하고, 리팩터링 후 구조 단위로 대조한다. 둘째, **시드 회귀** — 동일 시드 재실행으로 구조 변화를 CI에서 검출한다. 셋째, **judge 브릿지** — 출력 품질이 필요한 항목만 LLM-as-judge를 선택적으로 사용하되, judge 프롬프트·온도·모델 버전이 이벤트에 기록되어 judge 자체의 드리프트도 추적된다. Hermes가 ShareGPT 트래젝토리를 내보내는 것과 방향이 같지만, Hesmos는 내보내기뿐 아니라 "이 트래젝토리가 여전히 재현되는가"를 되돌려 검사하는 폐루프를 갖는다.

## 6. 코어 컴포넌트 상세 설계

### 6.1 Rust 크레이트 구조

코어는 6개 크레이트로 분해한다. swarms의 4,481행 god-class와 대비되는 구성이며, 각 크레이트는 P2 원칙에 따라 serde 기반 스키마 검증을 경계로 요구한다. workspace 루트의 Cargo.toml은 다음 멤버를 갖는다.

```text
hesmos/
  crates/
    hesmos-core/        # Task, Plan, Envelope, SessionHandle (schema)
    hesmos-orchestrator/ # GraphEngine, TopoWaves, seed PRNG
    hesmos-guard/       # Gate, PolicySet, HandoffContract validation
    hesmos-trace/       # EventLog (append-only, hash chain), Replay
    hesmos-budget/      # BudgetEnvelope, metering ledger
    hesmos-ffi/         # PyO3 bindings (hesmos-py import target)
  hesmos/               # CLI binary (clap): run/trace/replay/budget
  bindings/python/      # hesmos-py package (thin wrapper)
```

### 6.2 핵심 인터페이스

swarms가 덕 타이핑으로 흘려보낸 계약을 Rust는 타입으로 고정한다. 아래 4개 trait이 코어의 공개 계약이며, Python 계층은 이 계약의 구현체를 FFI 너머에서 호출할 뿐 직접 상태를 만지지 않는다.

```rust
/// 실행 단위: 좁은 프롬프트 + 좁은 도구셋의 명세 (로직 없음)
pub trait Stage: Send + Sync {
    fn id(&self) -> &StageId;
    fn profile(&self) -> &AgentProfile;      // 권한·모델·온도 명세
    fn input_schema(&self) -> &Schema;        // Envelope 검증
    fn done_criteria(&self) -> &DoneCriteria;
}

/// 그래프 엔진: 계획 컴파일 + 웨이브 스케줄 (결정론)
pub trait GraphEngine {
    fn compile(&self, plan: &Plan, seed: u64) -> Result<CompiledGraph, CompileError>;
    fn next_wave(&self, g: &CompiledGraph) -> Option<Wave>;
    fn commit(&self, g: &mut CompiledGraph, out: StageOutput) -> CommitReceipt;
}

/// 게이트: pre/post 판정, reason code 필수
pub trait Gate {
    fn check(&self, ctx: &GateCtx) -> GateVerdict; // Pass | Retry | Reject(reason)
}

/// 핸드오프 라우터: 계약 검증 + 루프 가드
pub trait HandoffRouter {
    fn route(&self, contract: HandoffContract) -> RouteDecision;
}
```

### 6.3 Python 바인딩 (hesmos-py)

Python 계층은 PyO3로 노출된 코어 API를 래핑한다. 사용자 관점에서 hesmos-py는 LLM·도구·메모리의 접착제이며, 코어가 요구하는 콜백(프로바이더 호출, 도구 실행)만 구현하면 된다. 아래는 GLM-5.3 flash를 1차 모델로 쓰는 최소 사용 예제다.

```python
import hesmos

core = hesmos.Session(seed=42, budget=hesmos.Budget(tokens=250_000))

plan = hesmos.Plan.from_yaml("""
stages:
  - id: research
    profile: {model: glm-5.3-flash, tools: [web.search, docs.read]}
  - id: draft
    depends: [research]
    profile: {model: glm-5.3-flash, tools: [write.file]}
  - id: verify
    depends: [draft]
    profile: {model: glm-5.3-flash, gates: [rubric.v1]}
""")

@core.provider("glm-5.3-flash")
def call_llm(req: hesmos.LlmRequest) -> hesmos.LlmReply:
    return glm_client.complete(req.messages, temperature=req.temperature)

receipt = core.run(plan, task="Draft a competitive feature report")
print(receipt.trace_id)   # hesmos trace replay <id> to reproduce
```

### 6.4 데이터 스키마와 CLI

핵심 스키마 4종이 코어를 관통한다. Envelope(노드 입출력 봉투), TraceEvent(관측 단위), HandoffContract(표 6), SessionHandle(세션 봉투)이며, 모두 serde/pydantic 이중 검증을 통과한다. Envelope는 swarms의 {role, content} dict를 타입 부착 구조로 승격한 것으로 id, from, to, type, payload, correlation_id, taint 플래그를 갖는다. CLI 명령은 표 8과 같다. trace 서브커맨드가 관측성의 공식 창구다.

**표 8. CLI 명령 설계**

| 명령 | 동작 | 비고 |
|---|---|---|
| hesmos run <plan> | 계획 실행(세션 개시, 예산 봉투 고정) | --seed, --budget, --dry-run |
| hesmos trace show <id> | 이벤트 타임라인 렌더 | --gate, --handoff 필터 |
| hesmos trace replay <id> | 동일 시드 재생(구조 단위) | --at step N 시간 여행 |
| hesmos budget <id> | 예산 소진·경고 이력 | 팀 단위 집계 지원 |
| hesmos eval <suite> | golden trace 회귀 실행 | CI 게이트용 exit code |
| hesmos serve | 게이트웨이 번들 기동(옵션) | HTTP/WS + 대시보드 API |

게이트웨이는 코어 위의 얇은 HTTP/WebSocket 어댑터로, 세션 API, 이벤트 스트리밍, 팀 예산과 감사 조회를 제공한다. Hermes류 25+ 메신저 어댑터는 게이트웨이 번들의 확장 슬롯으로 설계하되 1차 범위에서는 제외한다 — 접점 확장은 Footprint Ladder 순서를 따르고, 코어는 접점 독립성을 유지한다. 그림 2는 노드 간 handoff의 계약 라이프사이클을 요약한다.

![그림 2. 결정론적 Handoff 라이프사이클](./hesmos_assets/fig2_handoff.png)

**그림 2. 결정론적 Handoff 라이프사이클**

## 7. 실행 모델과 워크플로

### 7.1 턴 라이프사이클

한 턴은 그림 2의 4단계를 따른다. Plan & Dispatch(시드·예산 고정, 그래프 컴파일, G0 pre-flight), Agent Execution(격리 컨텍스트 윈도우에서 좁은 도구셋으로 실행, 전 단계 이벤트화), Handoff Contract Construction(계약 생성→검증→해시 체인 커밋), Verify & Integrate(post-gate, 루프 가드, 통합·봉인)이다. 단계 진행은 게이트 통과로만 가능하고, 어떤 단계든 실패 시 reason code와 함께 suspend+checkpoint 또는 bounded retry가 선택된다. 동기 루프(Hermes의 AIAgent)를 코어가 소유하지 않고 웨이브 스케줄러가 소유하므로, 단일 대화의 동기성과 서비스 백엔드의 비동기성이 공존한다. 노드 실행은 bathos 샌드박스에서, 스케줄·게이트·기록은 코어에서 일어난다.

### 7.2 4대 오케스트레이션 패턴

Hesmos는 패턴을 코어가 제공하는 그래프 템플릿으로 정규화한다. swarms의 14종 SwarmType과 달리 4종으로 압축하되, 각 패턴의 결정론 수준을 명시한다. 필요하면 flow DSL("a -> b, c")이 4종 템플릿의 조합 문법으로 해석된다.

**표 9. 패턴과 결정론 수준**

| 패턴 | 실행 모델 | 결정론 | 적합 작업 |
|---|---|---|---|
| Sequential | 선형 파이프라인, 계약 전달 | 완전(구조+경로) | 문서 생성, ETL |
| Parallel | fan-out + 통합 노드 | 완전(통합 순서 시드 고정) | 리서치, 코드베이스 스캔 |
| Swarm | 계약 기반 라우팅(라우터 노드 명시) | 경로 런타임 결정, 판정 기록 | 고객 지원, long-tail 요청 |
| Graph | DAG+게이트, 위상 웨이브 | 완전 | 검증 필수 파이프라인 |

### 7.3 장애 복구와 재개

세션 스냅샷(/save → _state/SESSION-SNAPSHOT.md)과 manifest 상태는 bathos 소유이지만, 체크포인트의 **단위**는 Hesmos가 정의한다. 커밋 지점(게이트 통과 직후 WAL 커밋)만이 체크포인트 후보이며, 게이트 통과 전 상태는 복원 대상에서 제외된다. 이는 Strands의 at-least-once 재개+멱등성 요구를 계승하되, 게이트로 1회 성공률을 끌어올린 변형이다. 복구 시 reason code 체계(MAX_HANDOFFS, TIMEOUT, REPETITIVE_HANDOFF, BUDGET_EXCEEDED, GATE_REJECT, PROVIDER_FAILURE)가 취소(CANCELLED)와 실패(FAILED)를 구분하므로, 재시도 정책이 상태별로 다르게 적용된다. 사용자는 "hesmos trace replay <id> --at step N"로 임의 커밋 지점에서 분기 재실행할 수 있다.

### 7.4 bathos 플랫폼 제어 흐름

그림 3은 실측한 bathos 두 평면 위에서 Hesmos 노드가 한 턴을 통과하는 흐름이다. 오케스트레이션 평면에서 슬래시 커맨드가 웨이브를 활성화하면 wave-engine이 동시성 캡(≤3, E-CONCURRENCY) 아래에서 Hesmos 노드를 역할로 스폰하고, model-plan.json이 세션 백엔드(런타임 glm이면 glm-5.3-flash)를 해석한다. 그 안에서 Hesmos는 pre-flight 게이트→LLM 호출→post-검증 게이트→WAL 커밋의 강제 지점을 운영한다. 실행 내내 careful-guard·freeze-guard 훅이 파괴 명령과 소유 경계 외 편집을 차단하고, audit-log 훅이 모든 도구 사용을 bathos 해시 체인에 기록한다. 턴 종료 시 Hesmos의 trace seal이 체인에 봉인되고 게이트 판정이 bathos gate verdict로 기록된다 — FAIL은 gate-enforce 훅이 exit 2로 다음 웨이브 진입을 물리 차단한다. Hermes 보완점 2.1에서 지적된 "실질적 안전 구성은 whole-process wrapping이며 운영 부담이 사용자에게 전가된다"는 문제를, bathos가 훅·엔진으로 플랫폼 차원에서 기본 제공하는 구조가 이 설계의 배포상 이점이다.

![그림 3. bathos 플랫폼 제어 흐름(오케스트레이션+엔진 평면)](./hesmos_assets/fig3_bathos.png)

**그림 3. bathos 플랫폼 제어 흐름(오케스트레이션+엔진 평면)**

## 8. 개발 로드맵과 리스크

### 8.1 Phase 마일스톤

로드맵은 4개 Phase로 나뉘며, 각 Phase의 완료 조건은 검증 가능한 산출물로 정의한다. GLM-5.3 flash는 전 Phase에서 구현 위임 주체이며, 태스크 카드(표 11)가 그 작업 분해다. Phase 0과 1이 끝나야 "결정론 코어 위에서 한 턴이 안전하게 도는" 최소 제품이 성립한다 — 이 순서가 반드시 지켜져야 한다.

**표 10. 개발 마일스톤**

| Phase | 범위 | 핵심 산출물 | 완료 조건 |
|---|---|---|---|
| 0. 기반 (2주) | 크레이트 골격, 스키마, FFI, CI | hesmos-core/trace 빌드, hesmos-py 임포트, golden trace 골격 | cargo test + pytest 통과, 해시 체인 무결성 테스트 |
| 1. 결정론 코어 (4주) | GraphEngine, Gate, HandoffContract, Budget | run/dry-run CLI, 계약 검증, 4중 안전장치 | 동일 시드 trace 해시 일치, ping-pong 감지 테스트 |
| 2. AI 계층 (3주) | GLM-5.3 flash 어댑터, MCP 클라이언트, 메모리 | End-to-end 예제 3종, 캐시 불변식 검증기 | 턴당 중복 토큰 상한 준수, taint 격리 테스트 |
| 3. 운영화 (3주) | Gateway, eval 하네스, replay UX, 문서 | serve 명령, CI 회귀 게이트, 태스크 카드 전량 소진 | eval 스위트 통과, 리플레이로 장애 3종 재현 |

### 8.2 GLM-5.3 flash 작업 카드

아래 카드는 bathos에서 GLM-5.3 flash에 그대로 던질 수 있는 단위 작업이다. 각 카드는 산출물·의존성·검증 방법을 명시하므로, 이슈 트래커에 복사해 붙이는 것으로 스프린트가 시작된다. 카드 순서는 의존 방향을 따른다.

**표 11. 작업 카드(요약)**

| ID | 작업 | 의존 | 검증 |
|---|---|---|---|
| T1 | hesmos-core 스키마(Task/Plan/Envelope/SessionHandle) + serde 테스트 | 없음 | 스키마 라운드트립 테스트 |
| T2 | TraceEvent 로그(append-only, 해시 체인) + tamper 감지 | T1 | 변조 감지 단위 테스트 |
| T3 | GraphEngine 컴파일+위상 웨이브+시드 PRNG | T1 | 동일 시드 웨이브 순서 동일성 |
| T4 | HandoffContract 검증기 + reason code 체계 | T1 | 필드 부재 시 reject 케이스 전수 |
| T5 | Gate 프레임워크(pre/post) + PolicySet | T3,T4 | 게이트 판정 이벤트화 확인 |
| T6 | hesmos-ffi(PyO3) + hesmos-py 래퍼 | T1,T2 | python -c "import hesmos" 통과 |
| T7 | CLI(clap): run/dry-run/trace show\|replay | T5,T6 | 예제 계획 1종 E2E |
| T8 | Budget 봉투+경고/초과 이벤트 | T5 | 80% 경고, 100% suspend 시나리오 |
| T9 | GLM 어댑터(bathos model-plan 정합)+usage 계측 | T6 | bathos model validate 통과, 호출당 토큰 계상 일치 |
| T10 | eval 하네스(golden trace+시드 회귀) | T7 | CI에서 회귀 검출 데모 |
| T11 | bathos 엔진 어댑터(state/gate/audit/model CLI 브리지) | T2,T5 | trace seal 후 bathos audit verify 통과 |
| T12 | 캐시 불변식 검증기(프롬프트 byte-stable 해시 검증·deferred 기본) — SS-18 | T6,T9 | 프롬프트 해시 검증·deferred 기본(--now opt-in) 테스트 — S3 |

### 8.3 리스크와 트레이드오프

설계의 반대급부를 정직하게 기록한다. 가장 큰 트레이드오프는 Hermes 2.6에서 이미 관찰된 것의 역방향이다 — Hesmos는 결정론과 캐시를 위해 런타임 유연성을 판다. 세션 중 역할 집합 동적 전환이 필요한 적응형 워크플로는 명시적 라우터 노드+deferred 명령으로 풀어야 하며, 즉석 전환이 필요한 조직에는 부적합할 수 있다. Rust 코어는 진입 장벽이지만, 제어 평면이 안정되면 Python 생태계의 속도를 그대로 누린다. PyO3 경계는 호출당 마셜링 비용이 있으나, LLM 호출 지연(수백 ms~수 s) 대비 수자원 수준이다.

**표 12. 리스크 매트릭스**

| 리스크 | 등급 | 영향 | 완화 전략 |
|---|---|---|---|
| 런타임 유연성 축소(캐시 불변식 대가) | 중 | 동적 전환 필요 워크플로 제한 | deferred+--now, 라우터 노드로 전환 명시화 |
| Rust 인력·진입 장벽 | 중 | 초기 개발 속도 | 코어 API 최소화, Python 계층에 확장 유도 |
| FFI 경계 버그(마셜링 불일치) | 중 | 크래시·데이터 불일치 | P2 이중 검증(serde+pydantic), FFI 퍼즈 테스트 |
| 계약 스키마의 과도한 경직 | 중 | 생산성 저하 | 필수 최소 필드 유지, failed_approaches 등 권장 필드 분리 |
| bathos 엔진 API 변경 | 낮 | 엔진 어댑터 재작업 | 엔진 CLI 표면 5종만 의존(표 4), 설치 진단으로 버전 정합 확인 |
| LLM 프로바이더 정책 변화 | 낮 | 캐시 동작 변화 | 3 API 모드 리졸버 유지, 프로바이더별 캐시 전략 캡슐화 |

다음 단계는 명확하다. bathos 작업 공간에 본 설계서의 크레이트 골격(T1·T2)을 세우고, bathos 엔진 어댑터(T11)로 감사 연계를 먼저 봉인하고, 표 1의 결정 사항을 리뷰 루브릭으로 등록한 뒤 Phase 0 스프린트를 개시한다. 이 문서의 표 5(매핑 매트릭스)·표 4(분계)와 그림 1~3은 개발 전 기간의 기준선으로 유지되며, 구현 과정에서 변경이 필요하면 해당 표를 먼저 고친 뒤 코드를 고친다.
