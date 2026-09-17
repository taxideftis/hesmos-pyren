# Hesmos User Guide (사용자 가이드)

> **Ἀσμός — 그리스어로 "벌집이 하늘로 날아오르는 순간."**
> 여러 에이전트가 일을 나눠 하되, 모든 결정은 재현 가능하고 모든 기록은 감사 가능해야 한다는 것이 Hesmos의 약속입니다.

이 가이드는 Hesmos를 AI 에이전트 실행기로 **가장 빨리, 가장 정확하게** 쓰는 방법을 다룹니다.
설계 배경이 궁금하면 [설계명세](pre/Agent_Hesmos_Spec_Arc.md)를, 요약은 [README](../README.md)를 참고하세요.

---

## 목차

1. [Hesmos란 무엇인가](#1-hesmos란-무엇인가)
2. [설치](#2-설치)
3. [5분 개념 잡기](#3-5분-개념-잡기)
4. [첫 실행 — 3단계](#4-첫-실행--3단계)
5. [계획 파일 작성법](#5-계획-파일-작성법)
6. [명령어 레퍼런스](#6-명령어-레퍼런스)
7. [출력 읽는 법](#7-출력-읽는-법)
8. [실패와 복구](#8-실패와-복구)
9. [예산 관리](#9-예산-관리)
10. [재현과 평가 (CI 연동)](#10-재현과-평가-ci-연동)
11. [Python에서 직접 임베딩하기](#11-python에서-직접-임베딩하기)
12. [게이트웨이·대시보드](#12-게이트웨이대시보드)
13. [트러블슈팅 FAQ](#13-트러블슈팅-faq)

---

## 1. Hesmos란 무엇인가

Hesmos는 **여러 AI 에이전트에게 일을 나눠 주는 오케스트레이터**입니다. 다만 일반 스웜 프레임워크와 결정적으로 다른 점이 하나 있습니다.

> **유연성은 엣지에서, 결정론은 웨이스트에서.**
> 계획 해석·스케줄·라우팅·상태 전이는 전부 Rust 코어가 소유해서, **시드만 고정하면 같은 실행이 같은 순서로 재현**됩니다. LLM이 코드를 실행하는 게 아니라, 코드가 LLM을 호출합니다.

이렇게 얻는 것:

| 얻는 것 | 의미 |
|---|---|
| **재현** | 같은 계획+같은 시드 = 같은 트레이스 해시. "어제는 됐는데 왜 지금은 안 되지?"가 사라집니다 |
| **감사** | 모든 상태 변화가 SHA-256 해시 체인에 기록 — 조용히 고쳐 쓰는 것이 구조적으로 불가능 |
| **비용 통제** | 예산 봉투가 80%에서 경고, 100%에서 자동 중단+체크포인트 |
| **명확한 실패** | 종료 코드 9개 대역으로 "왜 끝났는지"가 항상 기계 판독 가능 |

**이런 분께 적합합니다**: CI·야간 배치에서 에이전트를 무인 실행하는 개발자·소규모 팀. 대화형 챗봇을 찾고 있다면 Hesmos는 그 제품이 아닙니다.

---

## 2. 설치

### 2.1 필수 요소

| 요소 | 버전 | 비고 |
|---|---|---|
| Rust | `rust-toolchain.toml` 고정 | 저장소 클론 시 자동 적용 |
| Python | 3.11+ | AI 계층(FFI)용 |
| [maturin](https://www.maturin.rs/) | 최신 | PyO3 빌드 |
| bathos 런타임 | v0.4.0+ | *선택* — 감사 체인 연계(audit) 시에만 필요 |

### 2.2 빌드

```bash
# 저장소 클론
git clone https://github.com/taxideftis/hesmos-pyren.git
cd hesmos-pyren

# Rust 워크스페이스 (코어 + CLI)
cargo build --workspace

# Python AI 계층
pip install -e .

# 동작 확인
cargo test --workspace      # Rust 스위트
pytest                      # Python 스위트
```

> 🚧 **개발 상태**: Hesmos는 현재 구현이 진행 중입니다([CI 배지](https://github.com/taxideftis/hesmos-pyren/actions/workflows/ci.yml)가 살아 있는 기준입니다). 이 가이드의 명령어 표면은 이미 동결된 계약(`api-contracts.md` CLI-1~6)을 따르며, 명령어는 웨이브가 진행되며 순차적으로 활성화됩니다.

---

## 3. 5분 개념 잡기

| 개념 | 한 줄 정의 | 비유 |
|---|---|---|
| **계획 (Plan)** | 무엇을 할지 선언한 YAML 파일 — 스테이지 목록·의존·예산 | 레시피 |
| **세션 (Session)** | 계획을 실행한 한 번의 기록 단위 — 고유 `session_id` | 요리 1회 |
| **스테이지 (노드)** | 계획의 한 단계 — 에이전트 프로필(역할·모델·도구)을 가짐 | 레시피의 한 공정 |
| **핸드오프 계약** | 노드 사이를 넘겨주는 스키마 검증된 문서 — 문자열 전달 불가 | 공정 인수인계서 |
| **게이트 (Gate)** | 노드 전후의 품질 검문 — `PASS / CONCERNS / FAIL` 3값 | 품질 검사대 |
| **커밋 지점** | 진행이 저장된 지점(`→ commit #N`) — 되돌아갈 수 있는 앵커 | 게임 세이브 |
| **트레이스** | 세션의 전체 이벤트 기록(해시 체인) — `trace show`로 열람 | 비행기 블랙박스 |
| **예산 봉투** | 실행 전 고정되는 토큰/비용 한도 | 카드 한도 |
| **시드 (Seed)** | 실행의 난수 뿌리 — 같으면 결과도 같음 | 주사위 고정 |

**핵심 규칙 딱 하나만 기억하세요**: Hesmos에서 LLM은 다음 노드를 고르지 않습니다. 다음 노드는 계획(그래프)과 시드가 결정하고, LLM은 자기 노드의 목표 달성만 책집니다.

---

## 4. 첫 실행 — 3단계

### 1단계: dry-run — 공짜로 구조 검증

```console
$ hesmos run plan.hes --dry-run
컴파일 통과 — nodes=4  waves=3  외부 호출 0건 (비용 없음)

  wave 1  research
  wave 2  draft
  wave 3  verify

✓ 구조 검증 통과 — exit 0 (세션·이벤트 미생성)
  실행: hesmos run plan.hes --seed <시드> --budget tokens=<상한>
```

dry-run은 **세션도 이벤트도 WAL도 만들지 않고 LLM 호출도 0건**입니다. 계획 파일의 오탈자·참조 오류가 여기서 전부 걸립니다(위치까지 알려줍니다 — `파일:행:열`).

### 2단계: run — 시드와 예산을 주고 실행

```console
$ hesmos run plan.hes --seed 42 --budget tokens=250000
```

- `--seed`를 생략하면 OS 엔트로피로 생성되어 **세션 기록에 저장**됩니다 — 재현하려면 기록된 값을 쓰면 됩니다.
- `--budget`을 생략하면 무제한(단, 기록은 됩니다)입니다. 무인 실행에서는 **항상 지정하는 습관**을 권합니다.

### 3단계: trace show — 무슨 일이 있었나

```console
$ hesmos trace show <session_id>
$ hesmos trace show <session_id> --gate        # 게이트 판정만
$ hesmos trace show <session_id> --handoff     # 핸드오프만
```

렌더 전에 해시 체인을 검증합니다 — 체인이 깨져 있으면 보여주지 않고 실패합니다(exit 30). 그게 이 제품의 방식입니다.

---

## 5. 계획 파일 작성법

### 5.1 최소 예제 (Sequential)

```yaml
name: research-and-draft
task: "Rust 비동기 런타임 3종을 비교 리포트로 정리"   # 원 목표 — 필수 (goal_original)
pattern: Sequential

stages:
  - id: research
    profile:
      role: researcher
      model: glm-5.3-flash
      temperature: 0.2          # 고정 권장 — 재현에 직결
      tools: [web_search]       # 허용 목록 — 없으면 못 씀
    input_schema: task_brief
    done_criteria: "출처 3건 이상 인용"
    gates: {pre: default, post: default}
    is_terminal: false

  - id: draft
    profile:
      role: writer
      model: glm-5.3-flash
      temperature: 0.2
      tools: []
    input_schema: research_out
    done_criteria: "3단 구성 완료"
    gates: {pre: default, post: default}
    is_terminal: true           # 종료 선언 — 정확히 하나

edges: []                       # Sequential이라 생략 — 아래 DSL/Graph 참고
budget: {session_max_tokens: 250000}
```

### 5.2 4가지 패턴

| 패턴 | 언제 | 문법 |
|---|---|---|
| `Sequential` | 순서가 정해진 파이프라인 | 스테이지 나열 순서 |
| `Parallel` | 독립 작업 동시 실행 | `edges` 없는 스테이지 동시 배치 |
| `Swarm` | 협업 분배 | 팀 구성 선언 |
| `Graph` | 일반 DAG | `edges` 명시 또는 flow DSL |

flow DSL 한 줄로 의존을 표현할 수도 있습니다: `a -> b, c` (a 뒤에 b와 c가 병렬로).

### 5.3 작성 규칙 (컴파일러가 잡아주는 것들)

- `task`(원 목표) 누락 → **CE-05** 컴파일 오류
- 프로파일 없는 노드 → **CE-06**
- 존재하지 않는 노드 참조 → 위치(`파일:행:열`)와 함께 거부
- 노드 중복 ID → **CE-08**
- "적응적 라우팅"이 필요하면 런타임에 LLM이 경로를 고르는 게 아니라, **라우터 노드를 그래프에 명시**해야 합니다 (`is_router: true`)

컴파일 오류는 **CE-01~09** 코드로 분류되며, 모든 오류 메시지에 위치(`파일:행:열`)가 포함됩니다.

---

## 6. 명령어 레퍼런스

### `hesmos run` — 실행

```text
hesmos run <plan> [--seed <u64>] [--budget <spec>] [--dry-run]
```

| 플래그 | 값 | 설명 |
|---|---|---|
| `--seed` | u64 | 생략 시 OS 엔트로피(기록됨). **재현에는 기록값 사용** |
| `--budget` | `tokens=250000[,cost_usd=1.5]` | 봉투는 실행 전 고정 — 세션 중 변경 불가(설계 원칙) |
| `--dry-run` | — | 컴파일+스케줄만 출력. 세션·이벤트·WAL 미생성, 비용 0 |

- 성공 시 `.hesmos/sessions/<session_id>/`에 이벤트·체크포인트·응답 캐시가 기록됩니다.
- 같은 입력을 다시 실행하면 **새 세션**입니다 — 비교는 트레이스 해시로 하면 됩니다.

### `hesmos trace show` — 열람

```text
hesmos trace show <session_id> [--gate] [--handoff] [--limit N] [--json]
```

| 플래그 | 기본 | 설명 |
|---|---|---|
| `--gate` | — | 게이트 판정만 필터 |
| `--handoff` | — | 핸드오프만 필터 |
| `--limit` | 200 | 전체 렌더 방지 상한 |
| `--json` | — | 기계 판독 출력(도구 연동용) |

- 읽기 전용·멱등. 렌더 전 **체인 검증 실패 시 exit 30** — 변조된 기록은 재구성해 보여주지 않습니다.

### `hesmos trace replay` — 재현·재개

```text
hesmos trace replay <session_id> [--at step <N>]
```

| 항목 | 내용 |
|---|---|
| `--at step N` 생략 | 전체 재생 — 동일 4요소면 **트레이스 해시가 결정적으로 동일** |
| `--at step N` | N번 커밋 지점에서 분기 재실행 — fork가 새 세션으로 생성, `fork_of`에 계보 기록 |
| N이 커밋 지점이 아니면 | **exit 2** 거부 (`NOT_COMMIT_POINT`) — trace show의 `→ commit #N` 마커 위치를 쓰세요 |
| 예산 | 재개 세션도 `--budget`으로 새로 고정해야 합니다 |

> ⚠️ **`resume` 명령은 존재하지 않습니다.** 중단된 세션의 재개는 **언제나** `trace replay --at`입니다. 새 명령을 찾지 마세요 — 그게 설계입니다.

### `hesmos budget` — 소진 확인

```text
hesmos budget <session_id>          # 세션 단위
hesmos budget --team <team_id>      # 팀·세션·에이전트 3단위 집계
hesmos budget <session_id> --json   # 기계 판독
```

소진액·잔여·`budget.event` 이력을 미터 대장에서 보여줍니다. 읽기 전용·멱등.

### `hesmos eval` — 회귀 게이트

```text
hesmos eval <suite.yaml> [--bless <session_id>] [--json]
```

- 승인된 세션을 골든 샘플로 등록: `--bless <session_id>` (유일한 승인 표면)
- 스위트의 각 케이스를 재실행해 **구조**(노드 순서·게이트 판정)를 골든과 대조 — 자유 텍스트 비교가 아닙니다
- 불일치 검출 시 **exit 20** → CI에서 실패 처리

```yaml
# .github/workflows 예제 — 구조 회귀 감시
- run: hesmos eval eval/suites/my-suite.yaml
```

### `hesmos serve` — 게이트웨이

```text
hesmos serve [--bind <addr>] [--port <n>]
```

기본 `127.0.0.1:7330` (루프백 — 외부 바인딩 시 경고). SIGTERM에 우아하게 종료(exit 0), 포트 점유 시 exit 3. 자세한 것은 [§12](#12-게이트웨이대시보드).

---

## 7. 출력 읽는 법

### 7.1 세션 상태 7종

| 상태 | 심볼 | 색(TTY) | 의미 | 종료 코드 |
|---|---|---|---|---|
| INIT | `·` | — | 초기화 중 | — (진행 중) |
| RUNNING | `>` | — | 실행 중 | — (진행 중) |
| SUSPENDED | `‖` | yellow | 중단 — **재개 가능** (`replay --at`) | 10 (예산) · 130 (SIGINT) |
| HALTED | `✗` | red | 정지 — 정책 가드 (재개 가능) | 11 |
| ABORTED | — | red | 외부 의존 소진 | 12 |
| FAILED | — | red | 작업 판정 실패 | 20 |
| COMPLETED | `✓` | green | 완료 | 0 |

### 7.2 세 가지 규칙

1. **3색만 씁니다** — green=성공, yellow=경고, red=실패. 강조는 굵게.
2. **색은 단독 신호가 아닙니다** — 항상 심볼+단어가 함께 나옵니다 (`✗ FAIL GATE_REJECT`). 색각 터미널에서도 안전.
3. **같은 상태는 어디서나 같은 이름** — CLI든 CI 로그든 대시보드든 어휘 하나.

### 7.3 커밋 마커

트레이스에서 `→ commit #N` 행이 보이면 그것이 세이브 지점입니다. `trace replay --at step N`의 N은 이 번호입니다.

---

## 8. 실패와 복구

### 8.1 reason code 6종 — 원인과 처치

| 코드 | 계열 | 무슨 일 | 처치 |
|---|---|---|---|
| `BUDGET_EXCEEDED` | CANCELLED (10) | 예산 100% 도달 — 체크포인트 남음 | `budget` 상향 후 `replay --at` |
| `MAX_HANDOFFS` | CANCELLED (11) | 누적 핸드오프 20 초과 | 계획의 그래프를 단순화한 뒤 replay |
| `REPETITIVE_HANDOFF` | CANCELLED (11) | A→B→A 왕복 감지(윈도우 8) | 역할 경계를 분리하거나 라우터 명시 |
| `TIMEOUT` | CANCELLED (12) | 노드 시간 초과 (retry 2회 소진) | 타임아웃 상향 또는 작업 분할 |
| `PROVIDER_FAILURE` | CANCELLED (12) | LLM 호출 실패 (retry 2회 소진) | 프로바이더 상태 확인 후 replay |
| `GATE_REJECT` | **FAILED** (20) | 게이트가 최종 거부 | 계획·루브릭 수정 — **재시도로 못 고칩니다** |

### 8.2 기억할 구분 하나

> **CANCELLED(10·11·12·130)는 "재개가 복구", FAILED(20)는 "수정이 복구"입니다.**
> 코드만 봐도 CI에서 분기할 수 있습니다: 10·11·12·130 → 재시도 정책, 20 → 사람이 원인 조사.

### 8.3 자주 겪는 시나리오 3개

**① 야간 배치가 아침에 exit 10**
예산 소진입니다. `hesmos budget <session_id>`로 소진 확인 → `--budget tokens=<상향>`과 함께 `hesmos trace replay <session_id> --at step <마지막 커밋>`.

**② 에이전트 둘이 서로 일을 계속 넘김**
`REPETITIVE_HANDOFF`로 HALTED 됩니다(윈도우 8). 설계상 정상 작동입니다 — 두 노드의 `done_criteria`가 겹치지 않게 계획을 고치고 replay.

**③ 재실행했는데 결과가 어제와 다름**
시드가 다르면 경로가 다릅니다(정상). 어제의 것과 **같은지** 확인하려면 세션 기록의 시드로 재실행해 트레이스 해시를 비교하세요. 같은 시드+같은 계획인데 해시가 다르면 그건 버그입니다 — 저희가 고칠 것.

---

## 9. 예산 관리

- **봉투는 실행 전 고정**: `--budget tokens=250000[,cost_usd=1.5]`. 세션 중 변경은 불가능합니다(구조적으로).
- **80%에서 경고**(1회, 계속 진행), **100%에서 suspend+체크포인트**.
- 예산 판정은 **세션·팀·에이전트 3단위**로 집계됩니다 — `hesmos budget --team <team_id>`.
- judge 비용은 봉투 밖(스키마 수준에서 분리) — eval 스위트의 judge만 별도 과금 관점으로 보세요.

---

## 10. 재현과 평가 (CI 연동)

Hesmos의 재현은 **4요소**로 성립합니다: `session_id · plan_hash · seed · 응답 캐시`.

```bash
# 1) 베이스라인 실행 (시드 기록)
hesmos run plan.hes --seed 42 --budget tokens=250000

# 2) 승인 — 이 구조가 골든 샘플이 됨
hesmos eval eval/suites/core.yaml --bless <session_id>

# 3) CI에서 회귀 감시 — 구조가 바뀌면 exit 20
hesmos eval eval/suites/core.yaml
```

- 응답 캐시가 있으면 **바이트 단위** 재현, 없으면 **구조 단위** 재현입니다(노드 순서·게이트 판정).
- `eval`은 자유 텍스트를 비교하지 않습니다 — 흐릿한 승인이 구조가 무너지는 걸 보고 못 하는 게 포인트입니다.

---

## 11. Python에서 직접 임베딩하기

Rust 코어는 PyO3로 Python에 노출됩니다 (`import hesmos`). 계획 파일 대신 코드에서 세션을 다룰 때:

```python
import hesmos

# 세션 개시 + 계획 주입
handle = hesmos.session_open(...)
plan = hesmos.plan_from_yaml(open("plan.hes").read())

# 프롬프트 빌더 — 캐시 breakpoint(3계층) 자동 배치
messages = hesmos.core.build_messages(stage, snapshot)

# 설정 변경은 기본 deferred (다음 세션 적용) — 즉시는 명시적 opt-in
hesmos.core.apply_change(target)            # deferred (기본)
hesmos.core.apply_change(target, now=True)  # 즉시 — compression만이 유일한 무단 즉시 예외
```

기억할 규칙:

- `build_messages`가 반환한 프롬프트는 **byte-stable**이 보장됩니다 — 첫 build 후 system prompt·tool 정의를 바꾸려 하면 거부됩니다(타입으로 강제).
- 프롬프트 해시 검증은 코어가 매 턴 수행합니다 — Python 쪽에서 검증을 다시 만들지 마세요(이중 진실 원천 금지).
- 토큰 중복 측정기(`DuplicateTokenMeter`)는 기본 임계값이 없습니다 — 측정·기록만 하고, 상한은 운영 데이터가 생긴 뒤 정하는 것이 설계입니다.

---

## 12. 게이트웨이·대시보드

```console
$ hesmos serve                     # 127.0.0.1:7330
```

- **조회 전용**입니다. 실행·조작은 언제나 CLI가 소유합니다 — 대시보드에서 무언가를 "승인"하는 버튼은 없습니다.
- 세션 목록·예산·감사 데이터(코어 경유)·WS 이벤트 스트리밍 제공.
- 감사 데이터는 게이트웨이가 bathos에 직접 묻지 않고 **코어를 경유**합니다 — 감사 경로가 하나뿐이어야 하기 때문입니다.

---

## 13. 트러블슈팅 FAQ

**Q. `resume` 명령이 없나요?**
없습니다. `hesmos trace replay <session_id> --at step <N>`이 유일한 재개 경로입니다.

**Q. dry-run은 통과하는데 run이 바로 죽어요.**
dry-run은 외부 호출이 없으니 프로바이더 문제는 run에서 처음 보입니다. `PROVIDER_FAILURE`(exit 12)면 프로바이더 상태 확인, `3`(Compile)이면 계획 파일 위치 정보를 확인하세요.

**Q. exit 2가 뜨는데 문서와 값이 달라요.**
`hesmos`의 exit 2(Usage)와 `bathos` 엔진의 exit 2(게이트 차단)는 **서로 다른 바이너리의 값**입니다. 어느 명령이 내보냈는지 먼저 확인하세요.

**Q. 트레이스를 열었더니 exit 30으로 거부됩니다.**
로컬 체인 검증 실패(`EVIDENCE-INVALID`)입니다 — 기록이 변조되었거나 손상됐다는 뜻입니다. 변조된 증거를 그려주지 않는 것이 이 제품의 정직함입니다.

**Q. LLM이 실행 도중에 다른 노드를 부르게 할 수 있나요?**
설계상 불가능합니다(원칙 P1). 경로가 필요하면 `is_router: true` 라우터 노드를 그래프에 명시하세요 — 그 판정도 전부 이벤트로 기록됩니다.

**Q. 모델을 바꾸고 싶어요.**
스테이지 `profile.model`로 선언합니다(예: `glm-5.3-flash`). 런타임 백엔드 혼합은 거부됩니다 — 한 세션, 한 백엔드.

---

## 더 읽을거리

- [README](../README.md) — 프로젝트 소개·아키텍처·크레이트 지도
- [설계명세 (REV A)](pre/Agent_Hesmos_Spec_Arc.md) — 이 가이드 뒤에 있는 25페이지 계약
- [브랜딩](img/hesmos-brand-board.png) — Ἀσμός, 그리스어로 "벌집이 하늘로 날아오르는 순간"

---

*Hesmos User Guide · MIT License © 2026 ταξιδευτής*
