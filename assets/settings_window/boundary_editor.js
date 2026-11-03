const BOUNDARY_V3_UNITS = 65536;
const BOUNDARY_V3_COLORS = ['#37b983','#ec6875','#29a7dd','#9477db','#dda93f'];
const BOUNDARY_V3_EDGE_LABELS = {top:'上边',right:'右边',bottom:'下边',left:'左边'};
const BOUNDARY_V3_EDGES = ['top','right','bottom','left'];

function boundaryClone(value) {
  return typeof structuredClone === 'function'
    ? structuredClone(value)
    : JSON.parse(JSON.stringify(value));
}

function boundaryV3State() {
  if (!state.boundaryV3) {
    state.boundaryV3 = {
      view: 'spatial', draft: null, storageRevision: 0, savedDraftRevision: 0,
      saving: false, dirty: false, selectedScreen: '', selectedBoundary: '',
      automatic: null, undo: [], graphDrag: null, graphMotion: 'manual', graphVelocity: {},
      graphAnchors: {}, graphStoredPositions: {}, graphFrame: 0, graphLastTime: 0, spatialDrag: null,
      spatialPan: null, spatialViewport: null, portalDrag: null, peerDrag: null, previewT: .35,
      automaticVisible: true, manualScreen: '', correctionOpen: false,
      remoteSequence: 0, remoteKey: '', presentedRemoteKey: '', previewTimer: 0, previewLastSent: 0,
      completedKey: '', completeAfterSave: false, message: '', error: ''
    };
  }
  return state.boundaryV3;
}

function boundaryV3Peer(fingerprint) {
  return (currentModel().peers || []).find(peer => peer.fingerprint === fingerprint);
}

function boundaryV3PeerName(fingerprint) {
  if (fingerprint === currentModel().localFingerprint) return currentModel().language === 'en' ? 'Local device' : '本机';
  return boundaryV3Peer(fingerprint)?.label || fingerprint || '未知设备';
}

function boundaryV3PeerColor(fingerprint) {
  if (fingerprint === currentModel().localFingerprint) return '#5f9df6';
  const index = Math.max(0, (currentModel().peers || []).findIndex(peer => peer.fingerprint === fingerprint));
  return BOUNDARY_V3_COLORS[index % BOUNDARY_V3_COLORS.length];
}

function boundaryV3DefaultDraft(model = currentModel()) {
  const pixels = model.boundaryScreen || {};
  return {
    schemaVersion: 3,
    revision: 0,
    localFingerprint: String(model.localFingerprint || ''),
    screens: [{
      deviceFingerprint: String(model.localFingerprint || ''),
      pixelWidth: Math.max(0, Number(pixels.width) || 0),
      pixelHeight: Math.max(0, Number(pixels.height) || 0),
      physicalWidthMillimeters: Math.max(0, Number(pixels.physicalWidthMillimeters) || 0),
      physicalHeightMillimeters: Math.max(0, Number(pixels.physicalHeightMillimeters) || 0),
      rotationQuarterTurns: 0,
      canvasXMillimeters: 0,
      canvasYMillimeters: 0
    }],
    boundaries: []
  };
}

function boundaryV3RemoteKey(remote) {
  if (!remote) return '';
  return `${String(remote.initiatorFingerprint || '')}:${Math.max(0, Number(remote.sequence) || 0)}:${Math.max(0, Number(remote.layout?.revision) || 0)}:${String(remote.phase || 'preview')}:${String(remote.surface || 'spatial')}`;
}

function boundaryV3LoadModel(model = currentModel(), allowAutomaticNavigation = true, allowCompletionNavigation = allowAutomaticNavigation) {
  const editor = boundaryV3State();
  if (!['spatial','boundary','complete'].includes(editor.view)) editor.view = 'spatial';
  const workspace = model.boundaryWorkspaceV3;
  editor.storageRevision = Math.max(0, Number(workspace?.storageRevision) || 0);
  editor.savedDraftRevision = Math.max(0, Number(workspace?.draft?.revision) || 0);
  editor.draft = boundaryClone(workspace?.draft || boundaryV3DefaultDraft(model));
  const selectedStillKnown = editor.draft.screens.some(screen => screen.deviceFingerprint === editor.selectedScreen)
    || (model.peers || []).some(peer => peer.fingerprint === editor.selectedScreen);
  editor.selectedScreen = selectedStillKnown
    ? editor.selectedScreen
    : editor.draft.localFingerprint;
  editor.selectedBoundary = editor.draft.boundaries.some(boundary => boundary.id === editor.selectedBoundary)
    ? editor.selectedBoundary
    : (editor.draft.boundaries[0]?.id || '');
  editor.automatic = null;
  editor.dirty = false;
  editor.saving = false;
  editor.error = String(model.boundaryWorkspaceV3Error || '');
  const complete = Boolean(model.boundaryWorkspaceV3Applied);
  const remote = model.boundaryRemoteWorkspace;
  const remoteSequence = Math.max(0, Number(remote?.sequence) || 0);
  const remoteKey = boundaryV3RemoteKey(remote);
  const remoteInitiator = String(remote?.initiatorFingerprint || '');
  const newRemotePreview = remoteSequence > 0
    && remoteKey !== editor.presentedRemoteKey
    && remoteInitiator !== String(model.localFingerprint || '')
    && String(remote?.phase || 'preview') !== 'applied';
  const remoteView = remote?.surface === 'boundary' ? 'boundary' : 'spatial';
  const completeKey = complete ? `${editor.storageRevision}:${editor.savedDraftRevision}` : '';
  if (allowAutomaticNavigation && newRemotePreview) {
    if (editor.view !== remoteView) {
      editor.view = remoteView;
      state.scrollByPage.cross = 0;
      state.scrollResetPage = 'cross';
    }
  } else if (allowCompletionNavigation && complete && editor.completedKey !== completeKey) {
    editor.view = 'complete';
    editor.message = '';
    state.scrollByPage.cross = 0;
    state.scrollResetPage = 'cross';
  }
  if (remoteSequence > 0) editor.presentedRemoteKey = remoteKey;
  editor.completedKey = completeKey;
}

function currentBoundaryV3Draft() {
  const editor = boundaryV3State();
  if (!editor.draft) boundaryV3LoadModel();
  return editor.draft;
}

function acceptBoundaryModel(model) {
  const editor = boundaryV3State();
  const remoteSequence = Math.max(0, Number(model.boundaryRemoteWorkspace?.sequence) || 0);
  const remoteKey = boundaryV3RemoteKey(model.boundaryRemoteWorkspace);
  if (remoteSequence && remoteKey !== editor.remoteKey) editor.spatialViewport = null;
  editor.remoteSequence = remoteSequence;
  editor.remoteKey = remoteKey;
  const incoming = model.boundaryWorkspaceV3;
  const incomingStorage = Math.max(0, Number(incoming?.storageRevision) || 0);
  const saveCompleted = editor.saving && (
    (!editor.draft && Boolean(incoming))
    || incomingStorage !== editor.storageRevision
    || Number(incoming?.draft?.revision) !== editor.savedDraftRevision
  );
  const editing = state.interacting || editor.graphDrag || editor.spatialDrag || editor.spatialPan || editor.portalDrag || editor.peerDrag;
  if (!editing && (!editor.draft || saveCompleted || (!editor.dirty && !editor.saving))) {
    // A response to this machine's own edit may advance revisions or preserve an older applied
    // snapshot. Layout and boundary edits keep the operator on the current step. A local
    // confirmation may still open completion when it makes the pair fully confirmed.
    const completeAfterSave = editor.completeAfterSave;
    boundaryV3LoadModel(model, !saveCompleted, !saveCompleted || completeAfterSave);
    if (saveCompleted) editor.completeAfterSave = false;
  }
}

function boundaryV3SendSpatialPreview(immediate=false) {
  const editor=boundaryV3State();
  const transmit=()=>{
    editor.previewTimer=0;
    const layout=boundaryClone(editor.draft);
    layout.revision=editor.savedDraftRevision;
    layout.screens.forEach(screen=>{
      screen.canvasXMillimeters=Math.round(Number(screen.canvasXMillimeters)||0);
      screen.canvasYMillimeters=Math.round(Number(screen.canvasYMillimeters)||0);
    });
    if(boundaryV3ClientError(layout))return;
    editor.previewLastSent=performance.now();
    send({action:'preview-boundary-workspace-v3',boundaryLayout:layout});
  };
  if(immediate){if(editor.previewTimer)clearTimeout(editor.previewTimer);transmit();return;}
  const wait=Math.max(0,90-(performance.now()-editor.previewLastSent));
  if(!editor.previewTimer)editor.previewTimer=setTimeout(transmit,wait);
}

function boundarySaveFailed() {
  const editor = boundaryV3State();
  const wasSaving = editor.saving;
  if (wasSaving) boundaryV3LoadModel(currentModel(), false);
  editor.saving = false;
  editor.dirty = false;
  editor.completeAfterSave = false;
  if (wasSaving) editor.message = '保存失败，已恢复后台中的上一份草稿。';
  return wasSaving;
}

function boundarySaveSucceeded() {
  const editor = boundaryV3State();
  if (!editor.saving) return false;
  editor.saving = false;
  editor.dirty = false;
  editor.completeAfterSave = false;
  editor.error = '';
  return true;
}

function boundaryV3Screen(fingerprint, draft = currentBoundaryV3Draft()) {
  return draft.screens.find(screen => screen.deviceFingerprint === fingerprint);
}

function boundaryV3OrientedSize(screen) {
  const rotated = Number(screen.rotationQuarterTurns) % 2 === 1;
  return {
    width: rotated ? Number(screen.physicalHeightMillimeters) : Number(screen.physicalWidthMillimeters),
    height: rotated ? Number(screen.physicalWidthMillimeters) : Number(screen.physicalHeightMillimeters),
    pixelWidth: rotated ? Number(screen.pixelHeight) : Number(screen.pixelWidth),
    pixelHeight: rotated ? Number(screen.pixelWidth) : Number(screen.pixelHeight)
  };
}

function boundaryV3ScreenComplete(screen) {
  return Boolean(screen && Number(screen.pixelWidth) >= 2 && Number(screen.pixelHeight) >= 2
    && Number(screen.physicalWidthMillimeters) > 0 && Number(screen.physicalHeightMillimeters) > 0);
}

function boundaryV3DraftReady(draft = currentBoundaryV3Draft()) {
  return Boolean(draft.localFingerprint && draft.screens.length > 0 && draft.screens.every(boundaryV3ScreenComplete));
}

function boundaryV3PushUndo() {
  const editor = boundaryV3State();
  editor.undo.push(boundaryClone(editor.draft));
  if (editor.undo.length > 50) editor.undo.shift();
}

function boundaryV3MarkChanged(boundary) {
  boundary.mappingRevision = Math.max(1, Number(boundary.mappingRevision) || 0) + 1;
  boundary.localConfirmedRevision = 0;
  boundary.peerConfirmedRevision = 0;
}

function boundaryV3SegmentsOverlap(first, second) {
  return first.edge === second.edge
    && Number(first.start) < Number(second.end)
    && Number(second.start) < Number(first.end);
}

function boundaryV3PortalsOverlap(first, second) {
  return (first?.segments || []).some(left =>
    (second?.segments || []).some(right => boundaryV3SegmentsOverlap(left, right)));
}

function boundaryV3OverlapError(draft) {
  for (let left=0; left<draft.boundaries.length; left+=1) {
    for (let right=left+1; right<draft.boundaries.length; right+=1) {
      const first=draft.boundaries[left],second=draft.boundaries[right];
      if (boundaryV3PortalsOverlap(first.source,second.source)) {
        return '本机出口不能互相重叠；已保留拖动前的位置';
      }
      if (first.peerFingerprint===second.peerFingerprint
        && boundaryV3PortalsOverlap(first.target,second.target)) {
        return '同一对端的入口不能互相重叠；已保留拖动前的位置';
      }
    }
  }
  return '';
}

function boundaryV3ClientError(draft) {
  if (!boundaryV3DraftReady(draft)) return '请先补全工作区内每台屏幕的像素与毫米尺寸';
  if (draft.screens.length > 6) return '一个工作区最多包含本机和五台远端设备';
  const ids = new Set();
  for (const boundary of draft.boundaries) {
    if (!boundary.id || ids.has(boundary.id)) return '逻辑边界 ID 重复';
    ids.add(boundary.id);
    if (!boundary.source?.segments?.length || !boundary.target?.segments?.length) return '双端入口不能为空';
  }
  return boundaryV3OverlapError(draft);
}

function boundaryV3NormalizeRevisionsAgainstModel() {
  const editor = boundaryV3State();
  const workspace = currentModel().boundaryWorkspaceV3;
  const baseline = workspace?.draft;
  const storageRevision = Math.max(0, Number(workspace?.storageRevision) || 0);
  const draftRevision = Math.max(0, Number(baseline?.revision) || 0);
  if (storageRevision !== editor.storageRevision || draftRevision !== editor.savedDraftRevision) {
    boundaryV3LoadModel(currentModel(), false, false);
    editor.message = '后台草稿已更新，请在最新版本上重新调整。';
    editor.error = '';
    showToast(editor.message);
    render();
    return false;
  }
  if (!baseline) return true;
  const oldBoundaries = new Map((baseline.boundaries || []).map(boundary => [boundary.id, boundary]));
  const oldScreens = new Map((baseline.screens || []).map(screen => [screen.deviceFingerprint, screen]));
  const newScreens = new Map((editor.draft.screens || []).map(screen => [screen.deviceFingerprint, screen]));
  for (const boundary of editor.draft.boundaries) {
    const previous = oldBoundaries.get(boundary.id);
    if (!previous) {
      boundary.mappingRevision = 1;
      boundary.localConfirmedRevision = 0;
      boundary.peerConfirmedRevision = 0;
      continue;
    }
    const local = editor.draft.localFingerprint;
    const screenChanged = boundaryV3ScreenMappingKey(oldScreens.get(local)) !== boundaryV3ScreenMappingKey(newScreens.get(local))
      || boundaryV3ScreenMappingKey(oldScreens.get(boundary.peerFingerprint)) !== boundaryV3ScreenMappingKey(newScreens.get(boundary.peerFingerprint));
    const mappingChanged = boundaryV3BoundaryMappingKey(previous) !== boundaryV3BoundaryMappingKey(boundary);
    if (screenChanged || mappingChanged) {
      boundary.mappingRevision = Math.max(1, Number(previous.mappingRevision) || 0) + 1;
      boundary.localConfirmedRevision = 0;
      boundary.peerConfirmedRevision = 0;
    } else {
      // Drag quantisation and paired-handle updates can end at the authoritative geometry. A
      // semantic no-op must retain the old revision and confirmations; the store deliberately
      // rejects revision-only changes. The one intentional confirmation-only edit is the local
      // user confirming the current revision; peer confirmation remains backend-owned.
      boundary.mappingRevision = previous.mappingRevision;
      boundary.localConfirmedRevision = Number(boundary.localConfirmedRevision) === Number(previous.mappingRevision)
        ? previous.mappingRevision
        : previous.localConfirmedRevision;
      boundary.peerConfirmedRevision = previous.peerConfirmedRevision;
    }
  }
  return true;
}

function boundaryV3Save(message, completeAfterSave = false) {
  const editor = boundaryV3State();
  if (editor.saving) return;
  if (!boundaryV3NormalizeRevisionsAgainstModel()) return;
  editor.draft.revision = editor.savedDraftRevision;
  const error = boundaryV3ClientError(editor.draft);
  if (error) { editor.error = error; showToast(`操作未完成：${error}`); render(); return; }
  editor.dirty = true;
  editor.saving = true;
  editor.completeAfterSave = completeAfterSave;
  editor.message = message || '正在保存人工边界草稿…';
  send({
    action: 'save-boundary-workspace-v3',
    expectedStorageRevision: editor.storageRevision,
    surface: editor.view === 'spatial' ? 'spatial' : 'boundary',
    boundaryLayout: boundaryClone(editor.draft)
  });
  render();
}

function boundaryV3PerimeterMetrics(screen) {
  const size = boundaryV3OrientedSize(screen);
  return {width:size.width,height:size.height,total:2*(size.width+size.height)};
}

function boundaryV3SegmentInterval(screen, segment) {
  const metrics = boundaryV3PerimeterMetrics(screen);
  const unit = (segment.edge === 'top' || segment.edge === 'bottom') ? metrics.width : metrics.height;
  const start = Number(segment.start) / BOUNDARY_V3_UNITS * unit;
  const end = Number(segment.end) / BOUNDARY_V3_UNITS * unit;
  if (segment.edge === 'top') return {start,end};
  if (segment.edge === 'right') return {start:metrics.width+start,end:metrics.width+end};
  if (segment.edge === 'bottom') return {start:2*metrics.width+metrics.height-end,end:2*metrics.width+metrics.height-start};
  return {start:metrics.total-end,end:metrics.total-start};
}

function boundaryV3PortalArc(screen, portal) {
  const metrics = boundaryV3PerimeterMetrics(screen);
  const intervals = (portal?.segments || []).map(segment => boundaryV3SegmentInterval(screen,segment)).sort((a,b)=>a.start-b.start);
  if (!intervals.length) return {start:0,length:0,total:metrics.total};
  let largest = -1, cut = 0;
  for (let index=0; index<intervals.length; index+=1) {
    const next = index+1<intervals.length ? intervals[index+1].start : intervals[0].start+metrics.total;
    const gap = next-intervals[index].end;
    if (gap>largest) { largest=gap; cut=(index+1)%intervals.length; }
  }
  const ordered = intervals.slice(cut).concat(intervals.slice(0,cut).map(item=>({start:item.start+metrics.total,end:item.end+metrics.total})));
  return {start:ordered[0].start%metrics.total,length:ordered[ordered.length-1].end-ordered[0].start,total:metrics.total};
}

function boundaryV3ArcPortal(screen, start, length) {
  const metrics = boundaryV3PerimeterMetrics(screen);
  const total = metrics.total;
  start = ((start%total)+total)%total;
  length = clamp(length, .5, total-.5);
  const cuts = [0,metrics.width,metrics.width+metrics.height,2*metrics.width+metrics.height,total];
  const pieces = [];
  let cursor = start, remaining = length;
  while (remaining>.0001 && pieces.length<5) {
    const normalized = cursor%total;
    const nextCut = cuts.find(cut=>cut>normalized+.000001) ?? total;
    const actualIndex = Math.max(0,cuts.indexOf(nextCut)-1);
    const amount = Math.min(remaining,nextCut-normalized);
    const a = normalized, b = normalized+amount;
    let edge, naturalStart, naturalEnd, extent;
    if (actualIndex===0) { edge='top';extent=metrics.width;naturalStart=a;naturalEnd=b; }
    else if (actualIndex===1) { edge='right';extent=metrics.height;naturalStart=a-metrics.width;naturalEnd=b-metrics.width; }
    else if (actualIndex===2) { edge='bottom';extent=metrics.width;naturalStart=2*metrics.width+metrics.height-b;naturalEnd=2*metrics.width+metrics.height-a; }
    else { edge='left';extent=metrics.height;naturalStart=total-b;naturalEnd=total-a; }
    const segmentStart = Math.max(0,Math.round(naturalStart/extent*BOUNDARY_V3_UNITS));
    const segmentEnd = Math.min(BOUNDARY_V3_UNITS,Math.round(naturalEnd/extent*BOUNDARY_V3_UNITS));
    if (segmentEnd>segmentStart) pieces.push({edge,start:segmentStart,end:segmentEnd});
    remaining -= amount;
    cursor = (normalized+amount)%total;
  }
  return {segments:pieces};
}

function boundaryV3PointOnScreen(screen, perimeter) {
  const metrics = boundaryV3PerimeterMetrics(screen);
  const p = ((perimeter%metrics.total)+metrics.total)%metrics.total;
  if (p<=metrics.width) return {x:p/metrics.width,y:0,edge:'top'};
  if (p<=metrics.width+metrics.height) return {x:1,y:(p-metrics.width)/metrics.height,edge:'right'};
  if (p<=2*metrics.width+metrics.height) return {x:1-(p-metrics.width-metrics.height)/metrics.width,y:1,edge:'bottom'};
  return {x:0,y:1-(p-2*metrics.width-metrics.height)/metrics.height,edge:'left'};
}

function boundaryV3PointerPerimeter(event, element, screen) {
  const rect = element.getBoundingClientRect();
  const x = clamp((event.clientX-rect.left)/rect.width,0,1);
  const y = clamp((event.clientY-rect.top)/rect.height,0,1);
  const distances = [{edge:'top',d:y},{edge:'right',d:1-x},{edge:'bottom',d:1-y},{edge:'left',d:x}].sort((a,b)=>a.d-b.d);
  const metrics = boundaryV3PerimeterMetrics(screen);
  if (distances[0].edge==='top') return x*metrics.width;
  if (distances[0].edge==='right') return metrics.width+y*metrics.height;
  if (distances[0].edge==='bottom') return metrics.width+metrics.height+(1-x)*metrics.width;
  return 2*metrics.width+metrics.height+(1-y)*metrics.height;
}

function boundaryV3SegmentStyle(segment, color) {
  const start = Number(segment.start)/BOUNDARY_V3_UNITS*100;
  const length = (Number(segment.end)-Number(segment.start))/BOUNDARY_V3_UNITS*100;
  return `--start:${start};--length:${length};--peer:${color}`;
}

function boundaryV3HandleStyle(screen, perimeter) {
  const point = boundaryV3PointOnScreen(screen,perimeter);
  return `left:${point.x*100}%;top:${point.y*100}%`;
}

function boundaryV3PortalMarkup(screen, portal, color, selected, boundaryId, endpointPrefix='source') {
  const arc = boundaryV3PortalArc(screen,portal);
  const segments = (portal?.segments || []).map((segment,index)=>`<span class="boundary-v3-segment ${segment.edge} ${selected?'selected':''}" style="${boundaryV3SegmentStyle(segment,color)}" data-v3-boundary="${escapeHtml(boundaryId)}" data-v3-portal="${endpointPrefix}" data-v3-piece="${index}"></span>`).join('');
  if (!selected) return segments;
  return segments+`<button class="boundary-v3-handle start" style="${boundaryV3HandleStyle(screen,arc.start)};--peer:${color}" data-v3-handle="start" data-v3-boundary="${escapeHtml(boundaryId)}" data-v3-portal="${endpointPrefix}" aria-label="调整起点"></button><button class="boundary-v3-handle end" style="${boundaryV3HandleStyle(screen,arc.start+arc.length)};--peer:${color}" data-v3-handle="end" data-v3-boundary="${escapeHtml(boundaryId)}" data-v3-portal="${endpointPrefix}" aria-label="调整终点"></button>`;
}

function boundaryV3PendingPortalMarkup(portal) {
  return (portal?.segments || []).map(segment=>`<span class="boundary-v3-segment boundary-v3-pending-segment ${segment.edge}" style="${boundaryV3SegmentStyle(segment,'var(--muted)')}"></span>`).join('');
}

function boundaryV3Rect(screen) {
  const size = boundaryV3OrientedSize(screen);
  return {x:Number(screen.canvasXMillimeters),y:Number(screen.canvasYMillimeters),width:size.width,height:size.height};
}

function boundaryV3Center(rect) { return {x:rect.x+rect.width/2,y:rect.y+rect.height/2}; }
function boundaryV3Dot(a,b) { return a.x*b.x+a.y*b.y; }
function boundaryV3Corners(rect) { return [{x:rect.x,y:rect.y},{x:rect.x+rect.width,y:rect.y},{x:rect.x+rect.width,y:rect.y+rect.height},{x:rect.x,y:rect.y+rect.height}]; }
function boundaryV3ProjectionInterval(rect,axis) { const values=boundaryV3Corners(rect).map(point=>boundaryV3Dot(point,axis)); return {minimum:Math.min(...values),maximum:Math.max(...values)}; }
function boundaryV3PointAlong(origin,direction,distance) { return {x:origin.x+direction.x*distance,y:origin.y+direction.y*distance}; }

function boundaryV3RayRect(rect,origin,direction) {
  let enter=-Infinity,exit=Infinity;
  for (const axis of [{value:origin.x,direction:direction.x,minimum:rect.x,maximum:rect.x+rect.width},{value:origin.y,direction:direction.y,minimum:rect.y,maximum:rect.y+rect.height}]) {
    if (Math.abs(axis.direction)<.000001) { if (axis.value<axis.minimum || axis.value>axis.maximum) return null; continue; }
    let first=(axis.minimum-axis.value)/axis.direction,second=(axis.maximum-axis.value)/axis.direction;
    if (first>second) [first,second]=[second,first];
    enter=Math.max(enter,first);exit=Math.min(exit,second);
    if (enter>exit) return null;
  }
  return {enter,exit};
}

function boundaryV3ProjectionSupport(pair,value) {
  const origin={x:pair.normal.x*value,y:pair.normal.y*value};
  const localHit=boundaryV3RayRect(pair.localRect,origin,pair.direction),remoteHit=boundaryV3RayRect(pair.remoteRect,origin,pair.direction);
  if(!localHit||!remoteHit)return null;
  return {source:boundaryV3PointAlong(origin,pair.direction,localHit.exit),target:boundaryV3PointAlong(origin,pair.direction,remoteHit.enter)};
}

function boundaryV3Direction(localRect,remoteRect) {
  const local=boundaryV3Center(localRect),remote=boundaryV3Center(remoteRect);
  let dx=remote.x-local.x,dy=remote.y-local.y;
  const ax=Math.abs(dx),ay=Math.abs(dy),snap=Math.tan(10*Math.PI/180);
  if (ax>=ay) { if (ay<=Math.min(ax,Math.max(localRect.width,remoteRect.width))*snap) dy=0; }
  else if (ax<=Math.min(ay,Math.max(localRect.height,remoteRect.height))*snap) dx=0;
  const length=Math.hypot(dx,dy);
  return length<.0001?{x:1,y:0}:{x:dx/length,y:dy/length};
}

function boundaryV3EdgePoint(rect,edge,unit) {
  if (edge==='top') return {x:rect.x+rect.width*unit,y:rect.y};
  if (edge==='right') return {x:rect.x+rect.width,y:rect.y+rect.height*unit};
  if (edge==='bottom') return {x:rect.x+rect.width*unit,y:rect.y+rect.height};
  return {x:rect.x,y:rect.y+rect.height*unit};
}

function boundaryV3EdgeForPoint(rect,point) {
  return [{edge:'top',d:Math.abs(point.y-rect.y)},{edge:'right',d:Math.abs(point.x-rect.x-rect.width)},{edge:'bottom',d:Math.abs(point.y-rect.y-rect.height)},{edge:'left',d:Math.abs(point.x-rect.x)}].sort((a,b)=>a.d-b.d)[0].edge;
}

function boundaryV3EdgeUnit(rect,edge,point) {
  return (edge==='top'||edge==='bottom')?clamp((point.x-rect.x)/rect.width,0,1):clamp((point.y-rect.y)/rect.height,0,1);
}

function boundaryV3PerimeterForEdgeUnit(screen,edge,unit) {
  const metrics=boundaryV3PerimeterMetrics(screen);
  if(edge==='top')return unit*metrics.width;
  if(edge==='right')return metrics.width+unit*metrics.height;
  if(edge==='bottom')return metrics.width+metrics.height+(1-unit)*metrics.width;
  return 2*metrics.width+metrics.height+(1-unit)*metrics.height;
}

function boundaryV3ClockwiseRecords(records) {
  return BOUNDARY_V3_EDGES.flatMap(edge=>records.filter(record=>record.edge===edge).sort((first,second)=>(edge==='bottom'||edge==='left'?second.unit-first.unit:first.unit-second.unit)));
}

function boundaryV3LargestCyclicRun(records,predicate,screen) {
  if(!records.length)return[];
  const gap=records.findIndex(record=>!predicate(record));
  if(gap<0)return records.slice();
  const rotated=records.slice(gap+1).concat(records.slice(0,gap+1)),runs=[];
  let run=[];
  for(const record of rotated){
    if(predicate(record))run.push(record);
    else if(run.length){runs.push(run);run=[];}
  }
  if(run.length)runs.push(run);
  const size=boundaryV3OrientedSize(screen),weight=record=>(record.edge==='top'||record.edge==='bottom'?size.width:size.height)/220;
  return runs.sort((first,second)=>second.reduce((sum,record)=>sum+weight(record),0)-first.reduce((sum,record)=>sum+weight(record),0))[0]||[];
}

function boundaryV3PiecesFromRecords(records) {
  const pieces=[];
  for(const edge of BOUNDARY_V3_EDGES){
    const values=records.filter(record=>record.edge===edge).map(record=>record.unit).sort((a,b)=>a-b);
    if(!values.length)continue;
    let begin=values[0],previous=values[0];
    for(const value of values.slice(1).concat([NaN])){
      if(!Number.isFinite(value)||value-previous>2/220){
        const start=clamp(begin-1/440,0,1),end=clamp(previous+1/440,0,1);
        if(end-start>.002)pieces.push({edge,start:Math.round(start*BOUNDARY_V3_UNITS),end:Math.round(end*BOUNDARY_V3_UNITS)});
        begin=value;
      }
      previous=value;
    }
  }
  return pieces;
}

function boundaryV3MinimalPortal(screen,positions) {
  const metrics=boundaryV3PerimeterMetrics(screen),values=positions.map(value=>((value%metrics.total)+metrics.total)%metrics.total).sort((a,b)=>a-b);
  if(!values.length)return {segments:[]};
  let largestGap=-1,cut=0;
  for(let index=0;index<values.length;index+=1){
    const next=index+1===values.length?values[0]+metrics.total:values[index+1],gap=next-values[index];
    if(gap>largestGap){largestGap=gap;cut=index;}
  }
  const start=values[(cut+1)%values.length],length=metrics.total-largestGap,padding=Math.min(largestGap/4,Math.max(.5,Math.min(metrics.width,metrics.height)/220));
  if(values.length===1)return boundaryV3ArcPortal(screen,start-padding,2*padding);
  const corners=[0,metrics.width,metrics.width+metrics.height,2*metrics.width+metrics.height,metrics.total],snap=value=>{
    const candidates=corners.flatMap(corner=>[corner-metrics.total,corner,corner+metrics.total]).sort((first,second)=>Math.abs(first-value)-Math.abs(second-value));
    return Math.abs(candidates[0]-value)<=padding?candidates[0]:null;
  };
  const rawEnd=start+length,startCorner=snap(start),endCorner=snap(rawEnd),paddedStart=startCorner===null?start-padding:startCorner,paddedEnd=endCorner===null?rawEnd+padding:endCorner;
  return boundaryV3ArcPortal(screen,paddedStart,paddedEnd-paddedStart);
}

function boundaryV3Projection(draft=currentBoundaryV3Draft()) {
  if (!boundaryV3DraftReady(draft)) return {records:[],pairs:{},ranges:{},conflicts:[]};
  const localScreen=boundaryV3Screen(draft.localFingerprint,draft),localRect=boundaryV3Rect(localScreen);
  const peers=draft.screens.filter(screen=>screen.deviceFingerprint!==draft.localFingerprint);
  const pairs={};
  for (const screen of peers) {
    const remoteRect=boundaryV3Rect(screen),direction=boundaryV3Direction(localRect,remoteRect),normal={x:-direction.y,y:direction.x};
    const li=boundaryV3ProjectionInterval(localRect,normal),ri=boundaryV3ProjectionInterval(remoteRect,normal);
    const minimum=Math.max(li.minimum,ri.minimum),maximum=Math.min(li.maximum,ri.maximum),pair={screen,localRect,remoteRect,direction,normal,minimum,maximum};
    const span=Math.max(0,maximum-minimum),inset=Math.min(span/2,Math.max(.0001,span*.00001));
    pair.first=span>0?boundaryV3ProjectionSupport(pair,minimum+inset):null;
    pair.last=span>0?boundaryV3ProjectionSupport(pair,maximum-inset):null;
    pairs[screen.deviceFingerprint]=pair;
  }
  const normals={top:{x:0,y:-1},right:{x:1,y:0},bottom:{x:0,y:1},left:{x:-1,y:0}},records=[];
  for (const edge of BOUNDARY_V3_EDGES) for (let index=0;index<220;index+=1) {
    const unit=(index+.5)/220,source=boundaryV3EdgePoint(localRect,edge,unit),candidates=[];
    for (const fingerprint of Object.keys(pairs)) {
      const pair=pairs[fingerprint];
      if (boundaryV3Dot(pair.direction,normals[edge])<=.0001) continue;
      const hit=boundaryV3RayRect(pair.remoteRect,source,pair.direction);
      if (!hit||hit.enter<-.01) continue;
      const target=boundaryV3PointAlong(source,pair.direction,Math.max(0,hit.enter));
      const targetEdge=boundaryV3EdgeForPoint(pair.remoteRect,target);
      candidates.push({fingerprint,distance:hit.enter,targetEdge,targetUnit:boundaryV3EdgeUnit(pair.remoteRect,targetEdge,target)});
    }
    candidates.sort((a,b)=>a.distance-b.distance);
    records.push({edge,unit,source,candidates,chosen:candidates[0]||null});
  }
  const ranges={},orderedRecords=boundaryV3ClockwiseRecords(records);
  for (const screen of peers) {
    const fingerprint=screen.deviceFingerprint;
    const mappingRecords=boundaryV3LargestCyclicRun(orderedRecords,record=>record.chosen?.fingerprint===fingerprint,localScreen);
    const localMetrics=boundaryV3PerimeterMetrics(localScreen),targetMetrics=boundaryV3PerimeterMetrics(screen),edgeExtent=(value,edge)=>(edge==='top'||edge==='bottom'?value.width:value.height);
    let sourcePieces=[],targetPieces=[],reversed=false;
    if(mappingRecords.length){
      const firstRecord=mappingRecords[0],lastRecord=mappingRecords[mappingRecords.length-1],firstSource=boundaryV3PerimeterForEdgeUnit(localScreen,firstRecord.edge,firstRecord.unit),lastSource=boundaryV3PerimeterForEdgeUnit(localScreen,lastRecord.edge,lastRecord.unit),sourcePaddingStart=edgeExtent(localMetrics,firstRecord.edge)/440,sourcePaddingEnd=edgeExtent(localMetrics,lastRecord.edge)/440;
      sourcePieces=boundaryV3ArcPortal(localScreen,firstSource-sourcePaddingStart,boundaryV3ClockwiseDistance(firstSource,lastSource,localMetrics.total)+sourcePaddingStart+sourcePaddingEnd).segments;
      const targetPositions=mappingRecords.map(record=>boundaryV3PerimeterForEdgeUnit(screen,record.chosen.targetEdge,record.chosen.targetUnit)),targetPortal=boundaryV3MinimalPortal(screen,targetPositions),targetArc=boundaryV3PortalArc(screen,targetPortal),firstOffset=boundaryV3ClockwiseDistance(targetArc.start,targetPositions[0],targetMetrics.total),lastOffset=boundaryV3ClockwiseDistance(targetArc.start,targetPositions[targetPositions.length-1],targetMetrics.total);
      reversed=lastOffset<firstOffset;
      targetPieces=targetPortal.segments;
    }
    const rayRecords=boundaryV3LargestCyclicRun(orderedRecords,record=>record.candidates.some(candidate=>candidate.fingerprint===fingerprint),localScreen);
    ranges[fingerprint]={source:sourcePieces,target:targetPieces,pair:pairs[fingerprint],reversed,records:mappingRecords,rayRecords};
  }
  const conflicts=boundaryV3PiecesFromRecords(records.filter(record=>record.candidates.length>1));
  return {records,pairs,ranges,conflicts,localRect};
}

function boundaryV3StableId(fingerprint,index=0) {
  const compact=String(fingerprint).replace(/[^0-9A-Za-z]/g,'').slice(-16)||'peer';
  return `auto-${compact}-${index+1}`;
}

function boundaryV3AutomaticBoundaries() {
  const editor=boundaryV3State(),draft=editor.draft,result=boundaryV3Projection(draft),boundaries=[];
  draft.screens.filter(screen=>screen.deviceFingerprint!==draft.localFingerprint).forEach((screen,index)=>{
    const projected=result.ranges[screen.deviceFingerprint];
    if (!projected?.source?.length||!projected?.target?.length) return;
    boundaries.push({
      id:boundaryV3StableId(screen.deviceFingerprint,index),peerFingerprint:screen.deviceFingerprint,
      mappingRevision:1,localConfirmedRevision:1,peerConfirmedRevision:0,reversed:projected.reversed,
      source:{segments:projected.source},target:{segments:projected.target}
    });
  });
  editor.automatic={boundaries,result};
  return editor.automatic;
}

function boundaryV3GenerateDraft() {
  const editor=boundaryV3State();
  if (!boundaryV3DraftReady(editor.draft)||editor.draft.screens.length<2) { showToast('操作未完成：请先加入至少一台尺寸完整的远端屏幕'); return; }
  const previous=boundaryClone(editor.draft.boundaries);
  const automatic=boundaryV3AutomaticBoundaries();
  if(!automatic.boundaries.length){showToast('操作未完成：当前屏幕位置没有形成可用的自动投影');return;}
  automatic.boundaries.forEach(boundary=>{const existing=previous.find(candidate=>candidate.id===boundary.id);if(!existing)return;const changed=boundaryV3BoundaryMappingKey(existing)!==boundaryV3BoundaryMappingKey(boundary);boundary.mappingRevision=changed?Number(existing.mappingRevision)+1:Number(existing.mappingRevision);boundary.localConfirmedRevision=changed?boundary.mappingRevision:existing.localConfirmedRevision;boundary.peerConfirmedRevision=changed?0:existing.peerConfirmedRevision;});
  boundaryV3PushUndo();
  editor.draft.boundaries=boundaryClone(automatic.boundaries);
  editor.selectedBoundary=editor.draft.boundaries[0]?.id||'';
  editor.view='boundary';
  state.scrollByPage.cross=0;
  editor.message=`已生成 ${editor.draft.boundaries.length} 对双端入口；对端入口保持独立并等待确认。`;
  boundaryV3Save(editor.message);
  requestAnimationFrame(()=>{const content=document.querySelector('.content');if(content)content.scrollTop=0;});
}

function boundaryV3AddScreen(button) {
  const editor=boundaryV3State(),fingerprint=button.dataset.v3AddScreen;
  if (boundaryV3Screen(fingerprint,editor.draft)) return;
  const peer=boundaryV3Peer(fingerprint),info=peer?.screen||{},screen={
    deviceFingerprint:fingerprint,pixelWidth:Math.round(Number(info.pixelWidth)||0),pixelHeight:Math.round(Number(info.pixelHeight)||0),
    physicalWidthMillimeters:Math.round(Number(info.physicalWidthMillimeters)||0),physicalHeightMillimeters:Math.round(Number(info.physicalHeightMillimeters)||0),
    rotationQuarterTurns:0,canvasXMillimeters:0,canvasYMillimeters:0
  };
  const manual=button.closest('[data-v3-manual-screen-form]');
  if(manual){screen.physicalWidthMillimeters=Math.round(Number(manual.querySelector('[name="physicalWidth"]')?.value)||0);screen.physicalHeightMillimeters=Math.round(Number(manual.querySelector('[name="physicalHeight"]')?.value)||0);}
  if (!boundaryV3ScreenComplete(screen)) { showToast(manual?'操作未完成：请填写有效的物理毫米尺寸':'屏幕信息尚未自动同步；设备保持在线后会通过跨屏通道自动接收'); return; }
  const local=boundaryV3Screen(editor.draft.localFingerprint,editor.draft),localRect=boundaryV3Rect(local),screenSize=boundaryV3OrientedSize(screen),gap=80,index=editor.draft.screens.length-1,placements=[
    {x:localRect.x+localRect.width+gap,y:localRect.y+(localRect.height-screenSize.height)/2},
    {x:localRect.x+(localRect.width-screenSize.width)/2,y:localRect.y-screenSize.height-gap},
    {x:localRect.x+(localRect.width-screenSize.width)/2,y:localRect.y+localRect.height+gap},
    {x:localRect.x-screenSize.width-gap,y:localRect.y+(localRect.height-screenSize.height)/2},
    {x:localRect.x+localRect.width+gap,y:localRect.y-screenSize.height-gap}
  ],placement=placements[index%placements.length];screen.canvasXMillimeters=Math.round(placement.x);screen.canvasYMillimeters=Math.round(placement.y);
  boundaryV3PushUndo();editor.draft.screens.push(screen);editor.selectedScreen=fingerprint;editor.manualScreen='';editor.automatic=null;editor.spatialViewport=boundaryV3FitSpatialViewport(editor.draft);
  editor.message=`${peer?.label||'远端设备'} 已加入空间布局。`;
  boundaryV3Save(editor.message);
}

function boundaryV3RemoveScreen(fingerprint) {
  const editor=boundaryV3State();
  if (fingerprint===editor.draft.localFingerprint) return;
  boundaryV3PushUndo();
  editor.draft.screens=editor.draft.screens.filter(screen=>screen.deviceFingerprint!==fingerprint);
  editor.draft.boundaries=editor.draft.boundaries.filter(boundary=>boundary.peerFingerprint!==fingerprint);
  editor.selectedScreen=fingerprint;editor.selectedBoundary=editor.draft.boundaries[0]?.id||'';editor.correctionOpen=false;editor.automatic=null;editor.spatialViewport=boundaryV3FitSpatialViewport(editor.draft);
  boundaryV3Save('屏幕已从空间布局移除；设备配对和 SSH 保持不变。');
}

function boundaryV3RotateScreen(fingerprint) {
  const editor=boundaryV3State(),screen=boundaryV3Screen(fingerprint,editor.draft);if(!screen)return;
  boundaryV3PushUndo();screen.rotationQuarterTurns=(Number(screen.rotationQuarterTurns)+1)%4;
  editor.draft.boundaries.filter(boundary=>boundary.peerFingerprint===fingerprint||fingerprint===editor.draft.localFingerprint).forEach(boundaryV3MarkChanged);
  editor.automatic=null;boundaryV3Save(`${boundaryV3PeerName(fingerprint)} 已旋转 90°；相关映射等待重新确认。`);
}

function boundaryV3UpdateScreen(button) {
  const editor=boundaryV3State(),fingerprint=button.dataset.v3UpdateScreen,screen=boundaryV3Screen(fingerprint,editor.draft),card=button.closest('[data-v3-update-form]');if(!screen||!card)return;
  const read=name=>Math.round(Number(card.querySelector(`[name="${name}"]`)?.value)||0);
  const next={pixelWidth:read('pixelWidth'),pixelHeight:read('pixelHeight'),physicalWidthMillimeters:read('physicalWidth'),physicalHeightMillimeters:read('physicalHeight')};
  if(next.pixelWidth<2||next.pixelHeight<2||next.physicalWidthMillimeters<1||next.physicalHeightMillimeters<1){showToast('操作未完成：请填写有效的像素尺寸和物理毫米尺寸');return;}
  const mappingChanged=screen.pixelWidth!==next.pixelWidth||screen.pixelHeight!==next.pixelHeight||screen.physicalWidthMillimeters!==next.physicalWidthMillimeters||screen.physicalHeightMillimeters!==next.physicalHeightMillimeters;
  if(!mappingChanged)return;
  boundaryV3PushUndo();Object.assign(screen,next);
  editor.draft.boundaries.filter(boundary=>fingerprint===editor.draft.localFingerprint||boundary.peerFingerprint===fingerprint).forEach(boundaryV3MarkChanged);
  editor.automatic=null;boundaryV3Save(`${boundaryV3PeerName(fingerprint)} 的屏幕尺寸已保存；相关映射等待重新确认。`);
}

function boundaryV3GraphPositions(model=currentModel()) {
  const ids=[model.localFingerprint,...(model.peers||[]).map(peer=>peer.fingerprint)];
  const editor=boundaryV3State();
  let stored=editor.graphStoredPositions||{};
  if(!Object.keys(stored).length){try{stored=JSON.parse(localStorage.getItem('clipferry.boundaryGraph.v1')||'{}');}catch(_){stored={};}}
  const positions={};
  ids.forEach((id,index)=>{
    const angle=(index-1)*Math.PI*2/Math.max(1,ids.length-1)-Math.PI/2;
    positions[id]=stored[id]&&Number.isFinite(stored[id].x)&&Number.isFinite(stored[id].y)?stored[id]:index===0?{x:380,y:185}:{x:380+Math.cos(angle)*235,y:185+Math.sin(angle)*125};
  });
  return positions;
}

function boundaryV3SaveGraphPositions(positions) {
  const saved=Object.fromEntries(Object.entries(positions||{}).filter(([,point])=>Number.isFinite(point?.x)&&Number.isFinite(point?.y)).map(([id,point])=>[id,{x:point.x,y:point.y}]));
  boundaryV3State().graphStoredPositions=saved;
  send({action:'save-boundary-graph',graphPositions:saved});
}

function boundaryV3ShortName(value) { const text=String(value||'设备');return text.length>13?`${text.slice(0,11)}…`:text; }

function boundaryV3GraphMarkup(model) {
  const editor=boundaryV3State();
  const defaults=boundaryV3GraphPositions(model),validIds=[model.localFingerprint,...(model.peers||[]).map(peer=>peer.fingerprint)],previous=editor.graphPositions||{};editor.graphPositions=Object.fromEntries(validIds.map(id=>[id,previous[id]||defaults[id]]));
  const local=model.localFingerprint,peers=model.peers||[],nodes=[{fingerprint:local,label:boundaryV3PeerName(local),online:true,local:true},...peers];
  nodes.forEach(node=>{const point=editor.graphPositions[node.fingerprint];if(!editor.graphVelocity[node.fingerprint])editor.graphVelocity[node.fingerprint]={x:0,y:0};if(!editor.graphAnchors[node.fingerprint])editor.graphAnchors[node.fingerprint]={x:point.x,y:point.y};});
  const edges=peers.map(peer=>{const a=editor.graphPositions[local],b=editor.graphPositions[peer.fingerprint];return `<line class="boundary-v3-graph-edge ${peer.online?'online':''}" x1="${a.x}" y1="${a.y}" x2="${b.x}" y2="${b.y}" data-v3-edge-peer="${escapeHtml(peer.fingerprint)}"></line>`;}).join('');
  const nodeMarkup=nodes.map(node=>{const point=editor.graphPositions[node.fingerprint]||{x:380,y:185},color=node.local?'#5f9df6':boundaryV3PeerColor(node.fingerprint),title=node.local?node.label:`${node.label}${node.online?'（在线）':'（离线）'}`;return `<g class="boundary-v3-graph-node ${node.local?'local':''} ${node.online?'online':'offline'} ${editor.selectedScreen===node.fingerprint?'selected':''}" transform="translate(${point.x} ${point.y})" style="--peer:${color}" tabindex="0" role="button" data-v3-graph-node="${escapeHtml(node.fingerprint)}"><circle r="42"></circle><text y="2">${escapeHtml(boundaryV3ShortName(node.label))}</text><title>${escapeHtml(title)}</title></g>`;}).join('');
  const motion=editor.graphMotion||'manual';
  return `<div class="boundary-v3-embedded-graph"><div class="boundary-v3-embedded-graph-head"><span><strong>连接关系</strong><small>${nodes.length} 台设备 · ${peers.length} 条本机连接</small></span><span class="boundary-v3-graph-motion"><button class="button ${motion==='elastic'?'primary':''}" data-v3-graph-motion="elastic">弹性</button><button class="button ${motion==='manual'?'primary':''}" data-v3-graph-motion="manual">自由</button><button class="button" data-v3-action="reset-graph" aria-label="重置排布">重置</button></span></div><div class="boundary-v3-graph-stage embedded"><svg viewBox="0 0 760 370" role="group" aria-label="可拖动的设备连接关系图"><g class="boundary-v3-graph-edges">${edges}</g><g>${nodeMarkup}</g></svg></div></div>`;
}

function boundaryV3RefreshGraphDom() {
  const editor=boundaryV3State(),local=currentModel().localFingerprint,localPoint=editor.graphPositions?.[local];
  if(!localPoint)return;
  document.querySelectorAll('[data-v3-graph-node]').forEach(node=>{const point=editor.graphPositions[node.dataset.v3GraphNode];if(point)node.setAttribute('transform',`translate(${point.x} ${point.y})`);});
  document.querySelectorAll('[data-v3-edge-peer]').forEach(line=>{const peer=editor.graphPositions[line.dataset.v3EdgePeer];if(!peer)return;line.setAttribute('x1',localPoint.x);line.setAttribute('y1',localPoint.y);line.setAttribute('x2',peer.x);line.setAttribute('y2',peer.y);});
}

function boundaryV3ClampGraphPoint(point) {
  point.x=clamp(point.x,58,702);point.y=clamp(point.y,58,312);
}

function boundaryV3EnsureGraphAnimation() {
  const editor=boundaryV3State();if(editor.graphFrame)return;
  editor.graphLastTime=performance.now();editor.graphFrame=requestAnimationFrame(boundaryV3GraphPhysicsStep);
}

function boundaryV3GraphPhysicsStep(timestamp) {
  const editor=boundaryV3State();editor.graphFrame=0;
  if(state.page!=='cross'||!document.querySelector('.boundary-v3-graph-stage'))return;
  const ids=Object.keys(editor.graphPositions||{}),dragged=editor.graphDrag?.fingerprint,time=Math.min(2,Math.max(.35,(timestamp-editor.graphLastTime)/16.667||1));editor.graphLastTime=timestamp;
  ids.forEach(id=>{if(!editor.graphVelocity[id])editor.graphVelocity[id]={x:0,y:0};if(!editor.graphAnchors[id])editor.graphAnchors[id]={...editor.graphPositions[id]};});
  let motion=0;
  if(editor.graphMotion==='manual'){
    ids.forEach(id=>{if(id===dragged)return;const point=editor.graphPositions[id],velocity=editor.graphVelocity[id],anchor=editor.graphAnchors[id];velocity.x=(velocity.x+(anchor.x-point.x)*.13*time)*Math.pow(.7,time);velocity.y=(velocity.y+(anchor.y-point.y)*.13*time)*Math.pow(.7,time);if(Math.abs(velocity.x)<.015)velocity.x=0;if(Math.abs(velocity.y)<.015)velocity.y=0;point.x+=velocity.x*time;point.y+=velocity.y*time;boundaryV3ClampGraphPoint(point);motion+=Math.abs(velocity.x)+Math.abs(velocity.y)+Math.abs(anchor.x-point.x)*.08+Math.abs(anchor.y-point.y)*.08;});
  }else{
    const forces=Object.fromEntries(ids.map(id=>[id,{x:0,y:0}])),local=currentModel().localFingerprint;
    (currentModel().peers||[]).forEach(peer=>{const a=editor.graphPositions[local],b=editor.graphPositions[peer.fingerprint];if(!a||!b)return;let dx=b.x-a.x,dy=b.y-a.y,distance=Math.max(1,Math.hypot(dx,dy)),force=(distance-195)*.006,fx=dx/distance*force,fy=dy/distance*force;forces[local].x+=fx;forces[local].y+=fy;forces[peer.fingerprint].x-=fx;forces[peer.fingerprint].y-=fy;});
    for(let i=0;i<ids.length;i++)for(let j=i+1;j<ids.length;j++){const a=editor.graphPositions[ids[i]],b=editor.graphPositions[ids[j]],dx=b.x-a.x,dy=b.y-a.y,d2=Math.max(900,dx*dx+dy*dy),distance=Math.sqrt(d2),force=2400/d2,fx=dx/distance*force,fy=dy/distance*force;forces[ids[i]].x-=fx;forces[ids[i]].y-=fy;forces[ids[j]].x+=fx;forces[ids[j]].y+=fy;}
    ids.forEach(id=>{if(id===dragged)return;const point=editor.graphPositions[id],velocity=editor.graphVelocity[id];forces[id].x+=(380-point.x)*.0012;forces[id].y+=(185-point.y)*.0012;velocity.x=clamp((velocity.x+forces[id].x*time)*Math.pow(.84,time),-18,18);velocity.y=clamp((velocity.y+forces[id].y*time)*Math.pow(.84,time),-18,18);point.x+=velocity.x*time;point.y+=velocity.y*time;boundaryV3ClampGraphPoint(point);motion+=Math.abs(velocity.x)+Math.abs(velocity.y);});
  }
  boundaryV3RefreshGraphDom();
  if(editor.graphDrag||motion>.035)editor.graphFrame=requestAnimationFrame(boundaryV3GraphPhysicsStep);else boundaryV3SaveGraphPositions(editor.graphPositions);
}

function boundaryV3SetGraphMotion(mode) {
  if(!['manual','elastic'].includes(mode))return;
  const editor=boundaryV3State();editor.graphMotion=mode;editor.graphVelocity={};
  if(mode==='manual')editor.graphAnchors=Object.fromEntries(Object.entries(editor.graphPositions||{}).map(([id,point])=>[id,{x:point.x,y:point.y}]));
  render();if(mode==='elastic')boundaryV3EnsureGraphAnimation();
}

function boundaryV3SpatialViewport(draft) {
  const editor=boundaryV3State();
  if(!editor.spatialViewport)editor.spatialViewport=boundaryV3FitSpatialViewport(draft);
  return {...editor.spatialViewport};
}

function boundaryV3FitSpatialViewport(draft) {
  const rects=draft.screens.map(boundaryV3Rect);if(!rects.length)return {scale:1,x:40,y:40};
  const minX=Math.min(...rects.map(r=>r.x)),minY=Math.min(...rects.map(r=>r.y)),maxX=Math.max(...rects.map(r=>r.x+r.width)),maxY=Math.max(...rects.map(r=>r.y+r.height));
  const spanW=Math.max(1,maxX-minX),spanH=Math.max(1,maxY-minY),scale=clamp(Math.min(816/spanW,356/spanH),.4,2.2);
  return {scale,x:460-(minX+maxX)/2*scale,y:230-(minY+maxY)/2*scale};
}

function boundaryV3RemotePreview(model,draft) {
  const remote=model.boundaryRemoteWorkspace,source=remote?.layout;
  if(!source||source.localFingerprint!==draft.localFingerprint||!Array.isArray(source.screens))return null;
  const layout=boundaryClone(source),currentLocal=boundaryV3Screen(draft.localFingerprint,draft),remoteLocal=boundaryV3Screen(layout.localFingerprint,layout);
  if(!currentLocal||!remoteLocal)return null;
  const offsetX=Number(currentLocal.canvasXMillimeters)-Number(remoteLocal.canvasXMillimeters),offsetY=Number(currentLocal.canvasYMillimeters)-Number(remoteLocal.canvasYMillimeters);
  layout.screens.forEach(screen=>{screen.canvasXMillimeters=Number(screen.canvasXMillimeters)+offsetX;screen.canvasYMillimeters=Number(screen.canvasYMillimeters)+offsetY;});
  const peerScreen=layout.screens.find(screen=>screen.deviceFingerprint!==layout.localFingerprint);
  if(!peerScreen)return null;
  return {sequence:Number(remote.sequence)||0,phase:String(remote.phase||'preview'),layout,peerScreen,peerFingerprint:peerScreen.deviceFingerprint};
}

function boundaryV3RemotePreviewSvg(preview,viewport) {
  if(!preview)return'';
  const projection=boundaryV3Projection(preview.layout),pair=projection.pairs?.[preview.peerFingerprint],mapPoint=point=>({x:viewport.x+point.x*viewport.scale,y:viewport.y+point.y*viewport.scale});
  const rays=pair&&pair.maximum>pair.minimum?Array.from({length:5},(_,index)=>{const support=boundaryV3ProjectionSupport(pair,pair.minimum+(pair.maximum-pair.minimum)*(index+.5)/5);if(!support)return'';const source=mapPoint(support.source),target=mapPoint(support.target);return `<line class="boundary-v3-remote-ray" x1="${source.x}" y1="${source.y}" x2="${target.x}" y2="${target.y}"></line>`;}).join(''):'';
  const screen=preview.peerScreen,rect=boundaryV3Rect(screen),x=viewport.x+rect.x*viewport.scale,y=viewport.y+rect.y*viewport.scale,w=rect.width*viewport.scale,h=rect.height*viewport.scale,size=boundaryV3OrientedSize(screen),label=currentModel().language==='en'?'Peer live preview':'对端实时预览';
  return `<g class="boundary-v3-remote-preview" aria-label="${label}">${rays}<g transform="translate(${x} ${y})"><rect width="${w}" height="${h}" rx="7"></rect><text class="name" x="${w/2}" y="${h/2-5}">${escapeHtml(boundaryV3ShortName(boundaryV3PeerName(screen.deviceFingerprint)))}</text><text class="spec" x="${w/2}" y="${h/2+15}">${size.pixelWidth}×${size.pixelHeight} · ${size.width}×${size.height}mm</text></g></g>`;
}

function boundaryV3RemotePreviewBanner(preview) {
  if(!preview)return'';
  const english=currentModel().language==='en',hasBoundaries=Boolean(preview.layout.boundaries?.length),name=boundaryV3PeerName(preview.peerFingerprint);
  const title=english?`Live draft from ${name}`:`来自 ${name} 的实时草稿`;
  const copy=english?'Gray shows the peer’s pending layout. The solid layout remains active until both devices confirm and apply the update.':'浅灰色表示对端正在调整的新布局；实线布局继续保留，直到双方确认并应用新设置。';
  const action=english?(hasBoundaries?'Use and confirm this revision':'Use this layout'):(hasBoundaries?'采用并确认这版':'采用这份布局');
  return `<div class="boundary-v3-remote-banner"><span class="dot green"></span><div><strong>${escapeHtml(title)}</strong><small>${copy}</small></div><button class="button primary" data-v3-accept-remote="${preview.sequence}">${action}</button></div>`;
}

function boundaryV3SetSpatialZoom(nextScale,anchor={x:460,y:230}) {
  const editor=boundaryV3State(),viewport=boundaryV3SpatialViewport(editor.draft),scale=clamp(nextScale,.4,2.5),worldX=(anchor.x-viewport.x)/viewport.scale,worldY=(anchor.y-viewport.y)/viewport.scale;
  editor.spatialViewport={scale,x:anchor.x-worldX*scale,y:anchor.y-worldY*scale};
  render();
}

function boundaryV3SpatialPortalLines(ranges,width,height,color,focused,extraClass='') {
  return ranges.map(range=>{
    const start=Number(range.start)/BOUNDARY_V3_UNITS,end=Number(range.end)/BOUNDARY_V3_UNITS;
    let x1,y1,x2,y2;
    if(range.edge==='top'){x1=start*width;y1=0;x2=end*width;y2=0;}
    else if(range.edge==='right'){x1=width;y1=start*height;x2=width;y2=end*height;}
    else if(range.edge==='bottom'){x1=start*width;y1=height;x2=end*width;y2=height;}
    else{x1=0;y1=start*height;x2=0;y2=end*height;}
    return `<line class="boundary-v3-projection-segment ${focused?'focused':'dimmed'} ${extraClass}" style="--peer:${color}" x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}"></line>`;
  }).join('');
}

function boundaryV3SpatialPortalsMarkup(screen,projection,draft) {
  const editor=boundaryV3State(),rect=boundaryV3Rect(screen),viewport=editor.spatialDrag?.viewport||boundaryV3SpatialViewport(draft),width=rect.width*viewport.scale,height=rect.height*viewport.scale,local=screen.deviceFingerprint===draft.localFingerprint,color=boundaryV3PeerColor(screen.deviceFingerprint);
  if(local)return Object.entries(projection.ranges||{}).map(([fingerprint,value])=>boundaryV3SpatialPortalLines(value.source,width,height,boundaryV3PeerColor(fingerprint),editor.selectedScreen===draft.localFingerprint||editor.selectedScreen===fingerprint)).join('')+boundaryV3SpatialPortalLines(projection.conflicts||[],width,height,'#d99b28',true,'conflict');
  return boundaryV3SpatialPortalLines(projection.ranges?.[screen.deviceFingerprint]?.target||[],width,height,color,editor.selectedScreen===screen.deviceFingerprint);
}

function boundaryV3SpatialRaysMarkup(projection,viewport,draft) {
  const editor=boundaryV3State(),mapPoint=point=>({x:viewport.x+point.x*viewport.scale,y:viewport.y+point.y*viewport.scale}),remoteIds=draft.screens.filter(screen=>screen.deviceFingerprint!==draft.localFingerprint).map(screen=>screen.deviceFingerprint),selectedPeer=editor.selectedScreen===draft.localFingerprint?(remoteIds[0]||''):editor.selectedScreen;
  const markers=remoteIds.map((fingerprint,index)=>`<marker id="boundary-v3-arrow-${index}" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto"><path d="M 0 0 L 10 5 L 0 10 z" fill="${boundaryV3PeerColor(fingerprint)}"></path></marker>`).join('');
  const bands=remoteIds.map(fingerprint=>{const pair=projection.pairs?.[fingerprint];if(!pair?.first||!pair?.last||fingerprint!==selectedPeer)return'';const points=[pair.first.source,pair.first.target,pair.last.target,pair.last.source].map(mapPoint).map(point=>`${point.x},${point.y}`).join(' ');return `<polygon class="boundary-v3-ray-band focused" style="--peer:${boundaryV3PeerColor(fingerprint)}" points="${points}"></polygon>`;}).join('');
  const rays=remoteIds.map((fingerprint,peerIndex)=>{const pair=projection.pairs?.[fingerprint];if(!pair||pair.maximum<=pair.minimum)return'';const focused=fingerprint===selectedPeer,count=focused?5:4,color=boundaryV3PeerColor(fingerprint);return Array.from({length:count},(_,index)=>{const support=boundaryV3ProjectionSupport(pair,pair.minimum+(pair.maximum-pair.minimum)*(index+.5)/count);if(!support)return'';const source=mapPoint(support.source),target=mapPoint(support.target),primary=index===Math.floor(count/2);return `<line class="boundary-v3-ray ${focused?'focused':'dimmed'} ${primary?'primary':''}" style="--peer:${color}" x1="${source.x}" y1="${source.y}" x2="${target.x}" y2="${target.y}" ${primary?`marker-end="url(#boundary-v3-arrow-${peerIndex})"`:''}></line>`;}).join('');}).join('');
  return `<defs>${markers}</defs>${bands}${rays}`;
}

function boundaryV3RefreshSpatialDrag(drag) {
  const editor=boundaryV3State(),draft=editor.draft,projection=boundaryV3Projection(draft),viewport=drag.viewport;
  const rayLayer=document.querySelector('[data-v3-spatial-rays]');if(rayLayer)rayLayer.innerHTML=boundaryV3SpatialRaysMarkup(projection,viewport,draft);
  draft.screens.forEach(screen=>{const rect=boundaryV3Rect(screen),node=document.querySelector(`[data-v3-spatial-screen="${CSS.escape(screen.deviceFingerprint)}"]`);if(!node)return;node.setAttribute('transform',`translate(${viewport.x+rect.x*viewport.scale} ${viewport.y+rect.y*viewport.scale})`);node.classList.toggle('selected',editor.selectedScreen===screen.deviceFingerprint);const portals=node.querySelector('[data-v3-spatial-portals]');if(portals)portals.innerHTML=boundaryV3SpatialPortalsMarkup(screen,projection,draft);});
}

function boundaryV3SpatialMarkup(model) {
  const editor=boundaryV3State(),draft=editor.draft,preview=boundaryV3RemotePreview(model,draft),viewportDraft=preview?{...draft,screens:[...draft.screens,...preview.layout.screens.filter(screen=>screen.deviceFingerprint!==draft.localFingerprint)]}:draft,projection=boundaryV3Projection(draft),viewport=editor.spatialViewport||boundaryV3SpatialViewport(viewportDraft);
  if(!editor.spatialViewport)editor.spatialViewport=viewport;
  const rays=boundaryV3SpatialRaysMarkup(projection,viewport,draft);
  const remotePreviewSvg=boundaryV3RemotePreviewSvg(preview,viewport),remoteBanner=boundaryV3RemotePreviewBanner(preview);
  const screens=draft.screens.map(screen=>{const rect=boundaryV3Rect(screen),x=viewport.x+rect.x*viewport.scale,y=viewport.y+rect.y*viewport.scale,w=rect.width*viewport.scale,h=rect.height*viewport.scale,size=boundaryV3OrientedSize(screen),local=screen.deviceFingerprint===draft.localFingerprint,color=boundaryV3PeerColor(screen.deviceFingerprint),portals=boundaryV3SpatialPortalsMarkup(screen,projection,draft),corner=clamp(Math.min(w,h)*.085,8,15);return `<g class="boundary-v3-spatial-screen ${local?'local':''} ${editor.selectedScreen===screen.deviceFingerprint?'selected':''}" style="--peer:${color}" data-v3-spatial-screen="${escapeHtml(screen.deviceFingerprint)}" transform="translate(${x} ${y})" tabindex="0" role="button"><rect width="${w}" height="${h}" rx="7"></rect><g class="boundary-v3-spatial-guards"><path d="M0 0 H${corner} L0 ${corner} Z"></path><path d="M${w} 0 H${w-corner} L${w} ${corner} Z"></path><path d="M${w} ${h} H${w-corner} L${w} ${h-corner} Z"></path><path d="M0 ${h} H${corner} L0 ${h-corner} Z"></path></g><g data-v3-spatial-portals>${portals}</g><text class="orientation" x="18" y="24" transform="rotate(${Number(screen.rotationQuarterTurns)*90} 18 18)">↑</text><text class="name" x="${w/2}" y="${h/2-5}">${escapeHtml(boundaryV3ShortName(boundaryV3PeerName(screen.deviceFingerprint)))}</text><text class="spec" x="${w/2}" y="${h/2+15}">${size.pixelWidth}×${size.pixelHeight} · ${size.width}×${size.height}mm</text></g>`;}).join('');
  const selected=boundaryV3Screen(editor.selectedScreen,draft);
  const projectionStatus=draft.screens.length<2?'加入远端屏幕后才会生成投影':`${Object.values(projection.ranges||{}).filter(range=>range.source.length).length} 台远端设备得到自动投影${projection.conflicts.length?' · 黄色为近屏优先冲突区':''}`;
  const deviceCards=(model.peers||[]).map(peer=>{const screen=boundaryV3Screen(peer.fingerprint,draft),info=screen||peer.screen||{},pixels=Number(info.pixelWidth)>1&&Number(info.pixelHeight)>1,millimeters=Number(info.physicalWidthMillimeters)>0&&Number(info.physicalHeightMillimeters)>0,ready=pixels&&millimeters,driverMissing=pixels&&!millimeters&&peer.current&&peer.online,manual=editor.manualScreen===peer.fingerprint,color=boundaryV3PeerColor(peer.fingerprint),status=screen?`${info.physicalWidthMillimeters}×${info.physicalHeightMillimeters} mm · ${info.pixelWidth}×${info.pixelHeight}`:ready?'屏幕信息已同步':driverMissing?'对端系统未报告物理尺寸':peer.online?'正在通过加密设备连接获取屏幕信息…':'设备上线后自动获取屏幕信息',action=screen?`<div class="row boundary-v3-device-actions"><button class="button" data-v3-action="rotate" data-fingerprint="${escapeHtml(peer.fingerprint)}">旋转</button><button class="button danger ghost" data-v3-action="remove-screen" data-fingerprint="${escapeHtml(peer.fingerprint)}">移出布局</button><button class="button ghost" data-ssh="go" data-peer="${escapeHtml(peer.fingerprint)}">SSH</button></div>`:ready?`<button class="button primary boundary-v3-device-action" data-v3-add-screen="${escapeHtml(peer.fingerprint)}">加入空间布局</button>`:driverMissing?`<button class="button boundary-v3-device-action" data-v3-manual-screen-toggle="${escapeHtml(peer.fingerprint)}">${manual?'收起':'手动补充'}</button>`:'<button class="button boundary-v3-device-action" disabled>等待屏幕信息</button>',fallback=manual?`<div class="boundary-v3-manual-screen" data-v3-manual-screen-form><label>物理宽 mm<input class="input" name="physicalWidth" type="number" min="1" placeholder="例如 598"></label><label>物理高 mm<input class="input" name="physicalHeight" type="number" min="1" placeholder="例如 336"></label><button class="button primary" data-v3-add-screen="${escapeHtml(peer.fingerprint)}">保存并加入</button></div>`:'';return `<article class="boundary-v3-layout-device ${screen?'added':''} ${editor.selectedScreen===peer.fingerprint?'selected':''} ${peer.online?'':'offline'}" style="--peer:${color}" data-v3-select-screen="${escapeHtml(peer.fingerprint)}"><span class="boundary-v3-peer-bar"></span><div><div class="row"><strong>${escapeHtml(peer.label)}</strong><span class="dot ${peer.online?'green':''}"></span></div><small>${status}</small></div>${action}${fallback}</article>`;}).join('');
  const selectedEditor=selected?`<section class="boundary-v3-correction ${editor.correctionOpen?'open':''}"><button class="boundary-v3-correction-toggle" data-v3-action="toggle-correction" aria-expanded="${editor.correctionOpen?'true':'false'}"><span>${editor.correctionOpen?'▾':'▸'}</span>校正所选屏幕尺寸</button>${editor.correctionOpen?`<article class="boundary-v3-selected-screen" data-v3-update-form><strong>${escapeHtml(boundaryV3PeerName(selected.deviceFingerprint))}</strong><div class="boundary-v3-metric-grid"><label>像素宽<input class="input" name="pixelWidth" type="number" min="2" value="${selected.pixelWidth||''}"></label><label>像素高<input class="input" name="pixelHeight" type="number" min="2" value="${selected.pixelHeight||''}"></label><label>物理宽 mm<input class="input" name="physicalWidth" type="number" min="1" value="${selected.physicalWidthMillimeters||''}"></label><label>物理高 mm<input class="input" name="physicalHeight" type="number" min="1" value="${selected.physicalHeightMillimeters||''}"></label></div><span>当前方向 ${Number(selected.rotationQuarterTurns)*90}°</span><div class="row"><button class="button primary" data-v3-update-screen="${escapeHtml(selected.deviceFingerprint)}">保存校正</button>${selected.deviceFingerprint!==draft.localFingerprint?`<button class="button danger" data-v3-action="remove-screen" data-fingerprint="${escapeHtml(selected.deviceFingerprint)}">移出布局</button>`:''}</div></article>`:''}</section>`:'';
  const selectedSize=selected?boundaryV3OrientedSize(selected):null,selectedSummary=selected?`<div class="boundary-v3-layout-selection" style="--peer:${boundaryV3PeerColor(selected.deviceFingerprint)}"><span class="dot ${selected.deviceFingerprint===draft.localFingerprint||boundaryV3Peer(selected.deviceFingerprint)?.online?'green':''}"></span><div><strong>${escapeHtml(boundaryV3PeerName(selected.deviceFingerprint))}</strong><small>${selectedSize.pixelWidth}×${selectedSize.pixelHeight} · ${selectedSize.width}×${selectedSize.height} mm · 当前方向 ${Number(selected.rotationQuarterTurns)*90}°</small></div><span>拖动屏幕调整物理位置</span></div>`:'';
  return `<section class="boundary-v3-spatial-grid"><article class="card boundary-v3-map-card"><div class="boundary-v3-section-head"><div><h2>全局空间布局</h2><p>尺寸和方向自动同步；拖动每块屏幕摆放实际位置，投影会随位置实时更新。</p></div><span class="chip green">保护角仅作标记 · 不参与投影</span></div>${remoteBanner}${selectedSummary}<div class="boundary-v3-spatial-stage"><svg viewBox="0 0 920 460" aria-label="屏幕空间布局与自动投影" data-v3-spatial-svg><defs><pattern id="boundary-v3-grid" width="22" height="22" patternUnits="userSpaceOnUse"><circle cx="1" cy="1" r="1" fill="currentColor"></circle></pattern><pattern id="boundary-v3-guard-pattern" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="6" height="6" fill="var(--surface-raised)"></rect><rect width="2" height="6" fill="var(--border-strong)"></rect></pattern></defs><rect class="grid" width="920" height="460" fill="url(#boundary-v3-grid)" data-v3-spatial-pan></rect><g data-v3-spatial-world><g data-v3-spatial-rays>${rays}${remotePreviewSvg}</g><g>${screens}</g></g></svg><div class="boundary-v3-viewport-controls"><button class="button" data-v3-action="zoom-out" aria-label="缩小">−</button><strong>${Math.round(viewport.scale*100)}%</strong><button class="button" data-v3-action="zoom-in" aria-label="放大">＋</button><button class="button" data-v3-action="fit">适应全部</button></div></div><div class="boundary-v3-map-footer"><span>${projectionStatus} · 可拖动空白处平移，滚轮缩放</span><button class="button primary boundary-v3-generate-button" data-v3-action="generate" ${draft.screens.length<2||!boundaryV3DraftReady(draft)?'disabled':''}>生成边界草稿</button></div></article><aside class="card boundary-v3-side boundary-v3-device-panel"><div><h3>已配对设备</h3><p class="muted small">选择设备查看投影；屏幕信息同步后可直接加入布局。</p></div><div class="boundary-v3-layout-devices">${deviceCards||'<div class="empty">当前没有已配对设备</div>'}</div>${selectedEditor}</aside></section>`;
}

function boundaryV3BoundaryList(draft) {
  return draft.boundaries.map(boundary=>{const peer=boundaryV3Peer(boundary.peerFingerprint),selected=boundary.id===boundaryV3State().selectedBoundary,color=boundaryV3PeerColor(boundary.peerFingerprint),localOk=boundary.localConfirmedRevision===boundary.mappingRevision,peerOk=boundary.peerConfirmedRevision===boundary.mappingRevision,confirmed=localOk&&peerOk,status=confirmed?'双方已确认':!localOk?'等待本机确认':peer?.online?'等待对端确认':'对端离线';return `<button class="boundary-v3-boundary-card ${selected?'selected':''}" style="--peer:${color}" data-v3-select-boundary="${escapeHtml(boundary.id)}"><span class="boundary-v3-peer-bar"></span><span><strong>${escapeHtml(peer?.label||boundary.peerFingerprint)}</strong><small>${escapeHtml(boundary.id)} · 修订 ${boundary.mappingRevision}</small></span><em class="${confirmed?'confirmed':''}">${status}</em></button>`;}).join('');
}

function boundaryV3MiniScreen(screen,portal,color,label,marker,markerName) {
  const size=boundaryV3OrientedSize(screen),pieces=(portal?.segments||[]).map(segment=>`<span class="boundary-v3-mini-segment ${segment.edge}" style="${boundaryV3SegmentStyle(segment,color)}"></span>`).join(''),point=boundaryV3PointOnScreen(screen,marker);return `<div class="boundary-v3-mini-wrap"><strong>${escapeHtml(label)}</strong><div class="boundary-v3-mini-screen" style="aspect-ratio:${size.width}/${size.height};--peer:${color}">${pieces}<span class="boundary-v3-mini-marker" data-v3-pair-marker="${markerName}" style="left:${point.x*100}%;top:${point.y*100}%"></span></div></div>`;
}

function boundaryV3PairPanel(boundary,draft) {
  if(!boundary)return'';
  const editor=boundaryV3State(),local=boundaryV3Screen(draft.localFingerprint,draft),remote=boundaryV3Screen(boundary.peerFingerprint,draft),color=boundaryV3PeerColor(boundary.peerFingerprint),source=boundaryV3PortalArc(local,boundary.source),target=boundaryV3PortalArc(remote,boundary.target),t=editor.previewT,targetT=boundary.reversed?1-t:t,sourceMarker=source.start+source.length*t,targetMarker=target.start+target.length*targetT,peer=boundaryV3Peer(boundary.peerFingerprint),localOk=boundary.localConfirmedRevision===boundary.mappingRevision,peerOk=boundary.peerConfirmedRevision===boundary.mappingRevision;
  const status=!peer?.online?'对端离线':localOk&&peerOk?'双方已确认':!localOk?'等待本机确认':'等待对端确认';
  const english=currentModel().language==='en',peerName=boundaryV3PeerName(boundary.peerFingerprint),note=!peer?.online?(english?`This revision will sync automatically when ${peerName} reconnects.`:`${peerName} 上线后会自动收到这份修订。`):!localOk?(english?'Confirm this revision here first. It will then be sent to the peer automatically.':'请先点击“确认本机这一版”；系统随后会自动把同一修订发送给对端。'):!peerOk?(english?`“Waiting for peer confirmation” means the revision is already on ${peerName}. Open Cross-screen boundaries there and choose “Use and confirm this revision”. This page updates automatically when the confirmation returns.`:`“等待对端确认”表示这份修订已经自动发送到 ${peerName}。请在那台电脑的“跨屏边界”页点击“采用并确认这版”；确认回传后，本页会自动显示“双方已确认”。`):(english?'Both devices confirmed the same mapping revision.':'两台电脑已经确认同一个映射修订，可以进入最终应用。');
  return `<section class="card boundary-v3-pair"><div class="boundary-v3-section-head"><div><h3>双端入口映射</h3><p>${escapeHtml(boundary.id)} · 修订 ${boundary.mappingRevision} · ${boundary.reversed?'t → 1 − t':'t → t'}</p></div><span class="chip ${localOk&&peerOk?'green':'amber'}">${status}</span></div><div class="boundary-v3-pair-screens">${boundaryV3MiniScreen(local,boundary.source,color,`${boundaryV3PeerName(draft.localFingerprint)} · 出口`,sourceMarker,'source')}<div class="boundary-v3-pair-arrow">⇄<strong data-v3-pair-value>t ${t.toFixed(2)} → ${targetT.toFixed(2)}</strong></div>${boundaryV3MiniScreen(remote,boundary.target,color,`${boundaryV3PeerName(boundary.peerFingerprint)} · 入口`,targetMarker,'target')}</div><label class="boundary-v3-t">预览位置 t <input type="range" min="0" max="100" value="${Math.round(t*100)}" data-v3-preview-t><strong data-v3-pair-percent>${Math.round(t*100)}%</strong></label><div class="boundary-v3-pair-actions"><div class="row"><button class="button" data-v3-pair="back">对端逆时针</button><button class="button" data-v3-pair="forward">对端顺时针</button><button class="button" data-v3-pair="shorter">缩短</button><button class="button" data-v3-pair="longer">加长</button><button class="button" data-v3-pair="reverse">翻转方向</button></div><button class="button primary" data-v3-pair="confirm-local" ${localOk?'disabled':''}>${localOk?'本机已确认':'确认本机这一版'}</button></div><p class="boundary-v3-pair-note ${!peer?.online?'warning':''}">${escapeHtml(note)}</p></section>`;
}

function boundaryV3PendingPairPanel(preview) {
  const boundary=preview?.layout?.boundaries?.[0];if(!boundary)return'';
  const draft=preview.layout,local=boundaryV3Screen(draft.localFingerprint,draft),remote=boundaryV3Screen(boundary.peerFingerprint,draft);if(!local||!remote)return'';
  const editor=boundaryV3State(),color=boundaryV3PeerColor(boundary.peerFingerprint),source=boundaryV3PortalArc(local,boundary.source),target=boundaryV3PortalArc(remote,boundary.target),t=editor.previewT,targetT=boundary.reversed?1-t:t,sourceMarker=source.start+source.length*t,targetMarker=target.start+target.length*targetT,english=currentModel().language==='en';
  const title=english?'Peer mapping ready for your confirmation':'对端映射等待本机确认',status=english?'Confirm on this device':'等待本机采用',note=english?'The gray mapping is the revision already confirmed by the peer. Review both entrances, then use and confirm it on this device. It will take effect automatically after both confirmations.':'浅灰色映射是对端已经确认的修订。检查两端入口后，在本机采用并确认；双方确认齐全后会自动生效。',action=english?'Use and confirm this revision':'采用并确认这版';
  return `<section class="card boundary-v3-pair boundary-v3-pending-pair"><div class="boundary-v3-section-head"><div><h3>${title}</h3><p>${escapeHtml(boundary.id)} · ${english?'revision':'修订'} ${boundary.mappingRevision} · ${boundary.reversed?'t → 1 − t':'t → t'}</p></div><span class="chip amber">${status}</span></div><div class="boundary-v3-pair-screens">${boundaryV3MiniScreen(local,boundary.source,color,`${boundaryV3PeerName(draft.localFingerprint)} · ${english?'Exit':'出口'}`,sourceMarker,'source')}<div class="boundary-v3-pair-arrow">⇄<strong>t ${t.toFixed(2)} → ${targetT.toFixed(2)}</strong></div>${boundaryV3MiniScreen(remote,boundary.target,color,`${boundaryV3PeerName(boundary.peerFingerprint)} · ${english?'Entrance':'入口'}`,targetMarker,'target')}</div><div class="boundary-v3-pair-actions"><p>${note}</p><button class="button primary" data-v3-accept-remote="${preview.sequence}">${action}</button></div></section>`;
}

function boundaryV3ManualMarkup(model) {
  const editor=boundaryV3State(),english=model.language==='en',draft=editor.draft,preview=boundaryV3RemotePreview(model,draft),pendingPreview=preview?.layout?.boundaries?.length?preview:null,local=boundaryV3Screen(draft.localFingerprint,draft),selected=draft.boundaries.find(boundary=>boundary.id===editor.selectedBoundary),size=boundaryV3OrientedSize(local);
  const projected=boundaryV3Projection(draft),automaticReference=editor.automaticVisible?Object.entries(projected.ranges||{}).flatMap(([fingerprint,value])=>value.source.map(segment=>`<span class="boundary-v3-auto-segment ${segment.edge}" style="${boundaryV3SegmentStyle(segment,boundaryV3PeerColor(fingerprint))}"></span>`)).join(''):'';
  const boundaries=draft.boundaries.map(boundary=>boundaryV3PortalMarkup(local,boundary.source,boundaryV3PeerColor(boundary.peerFingerprint),boundary.id===editor.selectedBoundary,boundary.id)).join('');
  const pendingBoundaries=pendingPreview?pendingPreview.layout.boundaries.map(boundary=>boundaryV3PendingPortalMarkup(boundary.source)).join(''):'';
  const selectedPeer=selected?boundaryV3Peer(selected.peerFingerprint):null;
  const peerCards=(model.peers||[]).filter(peer=>boundaryV3Screen(peer.fingerprint,draft)).map(peer=>{const count=draft.boundaries.filter(boundary=>boundary.peerFingerprint===peer.fingerprint).length,active=selected?.peerFingerprint===peer.fingerprint;return `<article class="boundary-v3-drag-peer ${peer.online?'':'offline'} ${active?'selected':''}" draggable="true" data-v3-peer-drag="${escapeHtml(peer.fingerprint)}" data-v3-select-peer-boundary="${escapeHtml(peer.fingerprint)}" style="--peer:${boundaryV3PeerColor(peer.fingerprint)}"><span class="boundary-v3-peer-bar"></span><span><strong>${escapeHtml(peer.label)}</strong><small>${active?'正在调整 · ':''}${count} 条逻辑边界 · ${peer.online?'在线':'离线'}</small></span><button class="button ghost" data-ssh="go" data-peer="${escapeHtml(peer.fingerprint)}" draggable="false">SSH</button></article>`;}).join('');
  const focus=pendingPreview?`<div class="boundary-v3-manual-focus pending"><span class="dot green"></span><strong>${english?'Peer revision received':'已收到对端修订'}</strong><span>${english?'Review the gray boundary below':'请检查下方浅灰色边界'}</span></div>`:selected?`<div class="boundary-v3-manual-focus"><span class="dot ${selectedPeer?.online?'green':''}"></span><strong>正在调整 ${escapeHtml(selectedPeer?.label||selected.peerFingerprint)}</strong><span>${escapeHtml(selected.id)} · 修订 ${selected.mappingRevision}</span></div>`:'<div class="boundary-v3-manual-focus"><span class="dot"></span><strong>本机逻辑边界</strong><span>请先生成边界草稿</span></div>';
  const pairPanel=pendingPreview?boundaryV3PendingPairPanel(pendingPreview):selected?boundaryV3PairPanel(selected,draft):'<div class="boundary-v3-pair-empty">生成草稿后，在这里检查本机出口与对端入口的对应关系。</div>';
  const caption=pendingPreview?(english?'The gray boundary came from the peer and does not replace the current local settings until accepted.':'浅灰色边界来自对端，采用前不会覆盖本机现有设置。'):(english?'Dashed lines are the spatial-layout reference. Corner marks are hints; the 32-pixel guards are compiled only when applied.':'虚线是空间布局生成的参考。四角斜纹只作提示；32 像素保护区仅在最终应用时编译。');
  return `<section class="boundary-v3-manual-grid"><article class="card boundary-v3-manual"><div class="boundary-v3-section-head"><div><h2>本机边界</h2><p>在本机完整屏幕周长上调整出口；屏幕内部用于整理设备连接关系。</p></div><div class="row"><button class="button" data-v3-action="toggle-auto-reference">${editor.automaticVisible?'隐藏':'显示'}自动底稿</button><button class="button" data-v3-action="restore-auto" ${pendingPreview?'disabled':''}>恢复自动底稿</button><button class="button" data-v3-action="undo" ${editor.undo.length&&!pendingPreview?'':'disabled'}>Ctrl+Z 撤销</button><button class="button danger" data-v3-action="delete-boundary" ${selected&&!pendingPreview?'':'disabled'}>Delete 删除</button></div></div>${boundaryV3RemotePreviewBanner(preview)}${focus}<div class="boundary-v3-screen-stage"><div class="boundary-v3-edit-screen ${pendingPreview?'pending':''}" style="aspect-ratio:${size.width}/${size.height}" data-v3-edit-screen><span class="boundary-v3-corner tl"></span><span class="boundary-v3-corner tr"></span><span class="boundary-v3-corner br"></span><span class="boundary-v3-corner bl"></span>${boundaryV3GraphMarkup(model)}${automaticReference}${boundaries}${pendingBoundaries}</div></div><p class="boundary-v3-caption">${caption}</p>${pairPanel}</article><aside class="card boundary-v3-side boundary-v3-device-panel"><div><h3>已连接设备</h3><p class="muted small">选择设备查看其边界；拖到屏幕边缘可移动，按住 Ctrl 拖动可追加独立边界。</p></div><div class="boundary-v3-layout-devices">${peerCards||'<div class="empty">自动投影中还没有远端屏幕</div>'}</div><div class="divider"></div><div><h3>逻辑边界</h3><p class="muted small">点击任一片段都会选中整条跨角边界。</p></div><div class="boundary-v3-list">${boundaryV3BoundaryList(draft)||'<div class="empty">尚未生成边界草稿</div>'}</div><p class="boundary-v3-side-note">SSH、配对、剪贴板和普通文件功能独立于边界配置。</p></aside></section>`;
}

function boundaryV3ApplyBar(model) {
  const editor=boundaryV3State(),english=model.language==='en',draft=editor.draft,pending=draft.boundaries.filter(boundary=>boundary.localConfirmedRevision!==boundary.mappingRevision||boundary.peerConfirmedRevision!==boundary.mappingRevision),offline=draft.boundaries.filter(boundary=>boundaryV3Peer(boundary.peerFingerprint)?.online===false),runtimeReady=Boolean(model.boundaryWorkspaceV3RuntimeReady),canApply=runtimeReady&&draft.boundaries.length>0&&!pending.length&&!offline.length&&!editor.saving;
  let reason=canApply?(english?'Both confirmations are ready. The final boundaries are being applied automatically.':'双方确认已经齐全，系统正在自动应用最终边界。'):(english?'The final boundaries apply automatically and open the completion page after both devices confirm.':'等待双端确认完成后，系统会自动应用并进入完成页。');
  if(!runtimeReady) reason=english?'Final apply becomes available after every device supports paired confirmation. You can keep reviewing the draft.':'最终应用将在所有设备支持双端确认后开放；当前草稿可以继续保存和复查。';
  else if(offline.length) reason=english?`${offline.length} boundaries have an offline peer and cannot be applied.`:`${offline.length} 条边界的对端离线，不能应用。`;
  else if(pending.length) reason=english?`${pending.length} paired entrances are waiting for both confirmations.`:`${pending.length} 对双端入口等待双方确认。`;
  else if(!draft.boundaries.length) reason=english?'Generate at least one boundary draft from the spatial layout first.':'先从空间布局生成至少一条边界草稿。';
  return `<section class="card boundary-v3-apply"><div><span class="boundary-v3-step-dot">3</span><div><strong>${english?'Apply final boundaries automatically':'自动应用最终边界'}</strong><p>${reason}</p></div></div>${canApply?`<span class="chip green">${english?'Finishing':'正在完成'}</span>`:''}</section>`;
}

// Read-only schematic of the applied layout for the completion page. It reuses the spatial
// geometry and portal helpers but attaches no pointer handlers, so it cannot change the draft.
function boundaryV3CompleteDiagramMarkup(layout, english) {
  if(!layout?.screens?.length) return `<div class="boundary-v3-complete-diagram-empty">${english?'The schematic appears after the layout is applied.':'应用后这里显示只读的边界示意图。'}</div>`;
  const viewport=boundaryV3FitSpatialViewport({screens:layout.screens}),boundaries=layout.boundaries||[],label=english?'Applied boundary schematic':'已应用边界示意图';
  const screens=layout.screens.map(screen=>{
    const rect=boundaryV3Rect(screen),x=viewport.x+rect.x*viewport.scale,y=viewport.y+rect.y*viewport.scale,w=rect.width*viewport.scale,h=rect.height*viewport.scale,size=boundaryV3OrientedSize(screen),local=screen.deviceFingerprint===layout.localFingerprint,color=boundaryV3PeerColor(screen.deviceFingerprint);
    const portals=boundaries.map(boundary=>{
      const segments=local?boundary.source?.segments:(boundary.peerFingerprint===screen.deviceFingerprint?boundary.target?.segments:null);
      return segments?boundaryV3SpatialPortalLines(segments,w,h,boundaryV3PeerColor(boundary.peerFingerprint),true):'';
    }).join('');
    return `<g class="boundary-v3-complete-screen ${local?'local':''}" style="--peer:${color}" transform="translate(${x} ${y})"><rect width="${w}" height="${h}" rx="7"></rect>${portals}<text class="name" x="${w/2}" y="${h/2-5}">${escapeHtml(boundaryV3ShortName(boundaryV3PeerName(screen.deviceFingerprint)))}</text><text class="spec" x="${w/2}" y="${h/2+15}">${size.pixelWidth}×${size.pixelHeight} · ${size.width}×${size.height}mm</text></g>`;
  }).join('');
  return `<svg viewBox="0 0 920 460" role="img" aria-label="${label}">${screens}</svg>`;
}

function boundaryV3CompleteMarkup(model) {
  const workspace=model.boundaryWorkspaceV3,active=Boolean(model.boundaryWorkspaceV3Applied),layout=active?workspace?.applied?.layout:workspace?.draft,boundaries=layout?.boundaries||[],peers=new Set(boundaries.map(boundary=>boundary.peerFingerprint)),english=model.language==='en',multiPeerPending=active&&peers.size>1&&!model.boundaryMultiPeerRuntimeReady,activeFingerprint=model.connection?.activePeer||'',activePeer=model.peers?.find(peer=>peer.fingerprint===activeFingerprint),activePeerLabel=activePeer?.label||(english?'the active device':'当前活动设备');
  const title=active?(multiPeerPending?(english?'Boundary layout is applied':'边界布局已应用'):(english?'Cross-screen boundaries are active':'跨屏边界已生效')):(english?'Cross-screen boundaries are not active yet':'跨屏边界尚未生效'),copy=active?(multiPeerPending?(english?`All ${peers.size} device mappings are saved and confirmed. Live control currently uses ${escapeHtml(activePeerLabel)} only; the remaining boundaries will become available with multi-device runtime support.`:`${peers.size} 台设备的边界配置已经保存并确认。实时跨屏目前只启用 ${escapeHtml(activePeerLabel)}；其余边界将在多设备运行支持接入后启用。`):(english?'Every participating device confirmed the same mappings. The protected runtime snapshot is now in use.':'所有参与设备已经确认同一份映射，带四角保护的运行时快照现已生效。')):(english?'This page is a status preview. Finish the layout and paired entrance confirmations in Steps 1 and 2 before the new boundaries can take effect.':'这里显示的是完成状态预览。请先在第一、二步完成布局和双端入口确认，新边界才会生效。'),boundaryLabel=english?'Logical boundaries':'逻辑边界',deviceLabel=multiPeerPending?(english?'Configured devices':'已配置设备'):(english?'Participating devices':'参与设备'),guardLabel=english?'Corner protection':'四角保护',status=active?(multiPeerPending?(english?'Single-device control':'单设备运行'):(english?'Setup complete':'设置完成')):(english?'Not active':'尚未生效');
  return `<section class="card boundary-v3-complete ${active?'':'inactive'}"><div class="boundary-v3-complete-main"><span class="boundary-v3-complete-icon">${active?'✓':'!'}</span><div><span class="chip ${active?'green':'amber'}">${status}</span><h2>${title}</h2><p>${copy}</p></div><div class="boundary-v3-complete-facts"><div><strong>${boundaries.length}</strong><span>${boundaryLabel}</span></div><div><strong>${peers.size}</strong><span>${deviceLabel}</span></div><div><strong>${workspace?.applied?.cornerGuardPixels||32} px</strong><span>${guardLabel}</span></div></div></div><div class="boundary-v3-complete-schematic">${boundaryV3CompleteDiagramMarkup(active?layout:null,english)}</div></section>`;
}

function renderBoundaryEditor(model) {
  const editor=boundaryV3State();if(!editor.draft)boundaryV3LoadModel(model);
  const complete=Boolean(model.boundaryWorkspaceV3Applied),english=model.language==='en',activeLayout=complete?model.boundaryWorkspaceV3?.applied?.layout:null,activePeers=new Set((activeLayout?.boundaries||[]).map(boundary=>boundary.peerFingerprint)),multiPeerPending=complete&&activePeers.size>1&&!model.boundaryMultiPeerRuntimeReady;
  const status=editor.saving?(english?'Saving':'正在保存'):editor.error?(english?'Workspace error':'工作区异常'):multiPeerPending?(english?'Single-device control':'单设备运行'):complete?(english?'Active':'已生效'):(english?'Draft pending':'草稿未应用');
  const body=editor.view==='complete'?boundaryV3CompleteMarkup(model):editor.view==='spatial'?boundaryV3SpatialMarkup(model):boundaryV3ManualMarkup(model);
  return `<section class="boundary-v3-workspace"><header class="boundary-v3-product-head"><div><h2>跨屏边界</h2><p>在本机设置通向多台设备的屏幕位置、出口与对端入口。</p></div><button class="button boundary-v3-advanced-button" data-action="boundary-advanced-settings">高级设置</button></header><div class="boundary-v3-workflow card"><button class="boundary-v3-step ${editor.view==='spatial'?'active':''}" data-v3-view="spatial"><span>1</span><strong>自动投影</strong><small>摆放屏幕，生成建议范围</small></button><span class="boundary-v3-step-arrow">→</span><button class="boundary-v3-step ${editor.view==='boundary'?'active':''}" data-v3-view="boundary"><span>2</span><strong>双端调整</strong><small>检查出口、入口与方向</small></button><span class="boundary-v3-step-arrow">→</span><button class="boundary-v3-step ${editor.view==='complete'?'active':''}" data-v3-view="complete"><span>3</span><strong>应用最终边界</strong><small>查看当前生效状态</small></button><div class="boundary-v3-workflow-status"><span class="chip ${editor.error?'red':complete?'green':'amber'}">${status}</span></div></div>${editor.message?`<div class="boundary-v3-message ${editor.error?'error':''}">${escapeHtml(editor.error?runtimeErrorText(editor.error):editor.message)}</div>`:''}${body}${editor.view!=='complete'?boundaryV3ApplyBar(model):''}</section>`;
}

function boundaryV3FindBoundary(id) { return currentBoundaryV3Draft().boundaries.find(boundary=>boundary.id===id); }

function boundaryV3ClockwiseDistance(start,end,total) {
  return ((end-start)%total+total)%total;
}

function boundaryV3SharedNeighbor(boundary,endpoint,screen,draft) {
  const arc=boundaryV3PortalArc(screen,boundary.source),point=endpoint==='start'?arc.start:(arc.start+arc.length)%arc.total,tolerance=Math.max(.75,arc.total/100000);
  for(const candidate of draft.boundaries){
    if(candidate.id===boundary.id)continue;
    const other=boundaryV3PortalArc(screen,candidate.source),otherPoint=endpoint==='start'?(other.start+other.length)%other.total:other.start;
    const gap=Math.min(Math.abs(point-otherPoint),arc.total-Math.abs(point-otherPoint));
    if(gap<=tolerance)return {boundaryId:candidate.id,arc:other};
  }
  return null;
}

function boundaryV3ShiftTarget(boundary,action) {
  const editor=boundaryV3State(),screen=boundaryV3Screen(boundary.peerFingerprint,editor.draft),arc=boundaryV3PortalArc(screen,boundary.target),step=arc.total*.04;
  const before=boundaryClone(editor.draft),mappingBefore=boundaryV3BoundaryMappingKey(boundary);
  if(action==='back')arc.start-=step;
  if(action==='forward')arc.start+=step;
  if(action==='shorter')arc.length=Math.max(step,arc.length-step);
  if(action==='longer')arc.length=Math.min(arc.total-step,arc.length+step);
  if(action==='reverse')boundary.reversed=!boundary.reversed;
  if(action!=='reverse')boundary.target=boundaryV3ArcPortal(screen,arc.start,arc.length);
  if(mappingBefore===boundaryV3BoundaryMappingKey(boundary)){editor.draft=before;render();return;}
  const overlap=boundaryV3OverlapError(editor.draft);
  if(overlap){editor.draft=before;editor.message=overlap;editor.error='';showToast(`操作未完成：${overlap}`);render();return;}
  editor.undo.push(before);if(editor.undo.length>50)editor.undo.shift();
  boundaryV3MarkChanged(boundary);
  boundaryV3Save('对端入口已调整；本机出口保持原位，当前修订等待确认。');
}

function boundaryV3ConfirmLocal(boundary) {
  boundaryV3PushUndo();boundary.localConfirmedRevision=boundary.mappingRevision;
  boundaryV3Save('本机已确认这版入口；仍需对端确认同一修订。', true);
}

function boundaryV3DeleteSelected() {
  const editor=boundaryV3State();if(!editor.selectedBoundary)return;
  boundaryV3PushUndo();editor.draft.boundaries=editor.draft.boundaries.filter(boundary=>boundary.id!==editor.selectedBoundary);editor.selectedBoundary=editor.draft.boundaries[0]?.id||'';
  boundaryV3Save('整条逻辑边界已删除；配对、SSH 和对端入口配置保持独立。');
}

function boundaryV3Undo() {
  const editor=boundaryV3State();if(editor.saving)return;const previous=editor.undo.pop();if(!previous)return;
  const current=editor.draft;
  previous.boundaries.forEach(boundary=>{
    const existing=current.boundaries.find(candidate=>candidate.id===boundary.id);if(!existing)return;
    const previousLocal=boundaryV3Screen(previous.localFingerprint,previous),currentLocal=boundaryV3Screen(current.localFingerprint,current),previousPeer=boundaryV3Screen(boundary.peerFingerprint,previous),currentPeer=boundaryV3Screen(existing.peerFingerprint,current);
    const screenChanged=boundaryV3ScreenMappingKey(previousLocal)!==boundaryV3ScreenMappingKey(currentLocal)||boundaryV3ScreenMappingKey(previousPeer)!==boundaryV3ScreenMappingKey(currentPeer);
    const geometryChanged=JSON.stringify({peer:boundary.peerFingerprint,reversed:boundary.reversed,source:boundary.source,target:boundary.target})!==JSON.stringify({peer:existing.peerFingerprint,reversed:existing.reversed,source:existing.source,target:existing.target});
    if(screenChanged||geometryChanged){boundary.mappingRevision=Number(existing.mappingRevision)+1;boundary.localConfirmedRevision=0;boundary.peerConfirmedRevision=0;}
    else boundary.mappingRevision=existing.mappingRevision;
  });
  editor.draft=previous;editor.selectedBoundary=editor.draft.boundaries.some(boundary=>boundary.id===editor.selectedBoundary)?editor.selectedBoundary:(editor.draft.boundaries[0]?.id||'');
  boundaryV3Save('已按整条逻辑边界撤销上一步。');
}

function boundaryV3ScreenMappingKey(screen) {
  if(!screen)return'';
  return [screen.pixelWidth,screen.pixelHeight,screen.physicalWidthMillimeters,screen.physicalHeightMillimeters,screen.rotationQuarterTurns].join(':');
}

function boundaryV3BoundaryMappingKey(boundary) {
  if(!boundary)return'';
  return JSON.stringify({peerFingerprint:boundary.peerFingerprint,reversed:Boolean(boundary.reversed),source:boundary.source,target:boundary.target});
}

function boundaryV3RefreshPairPreview(input) {
  const editor=boundaryV3State(),boundary=boundaryV3FindBoundary(editor.selectedBoundary),panel=input.closest('.boundary-v3-pair');
  if(!boundary||!panel)return;
  editor.previewT=Number(input.value)/100;
  const draft=editor.draft,local=boundaryV3Screen(draft.localFingerprint,draft),remote=boundaryV3Screen(boundary.peerFingerprint,draft),source=boundaryV3PortalArc(local,boundary.source),target=boundaryV3PortalArc(remote,boundary.target),targetT=boundary.reversed?1-editor.previewT:editor.previewT;
  const sourcePoint=boundaryV3PointOnScreen(local,source.start+source.length*editor.previewT),targetPoint=boundaryV3PointOnScreen(remote,target.start+target.length*targetT),sourceMarker=panel.querySelector('[data-v3-pair-marker="source"]'),targetMarker=panel.querySelector('[data-v3-pair-marker="target"]');
  if(sourceMarker){sourceMarker.style.left=`${sourcePoint.x*100}%`;sourceMarker.style.top=`${sourcePoint.y*100}%`;}
  if(targetMarker){targetMarker.style.left=`${targetPoint.x*100}%`;targetMarker.style.top=`${targetPoint.y*100}%`;}
  const value=panel.querySelector('[data-v3-pair-value]'),percent=panel.querySelector('[data-v3-pair-percent]');
  if(value)value.textContent=`t ${editor.previewT.toFixed(2)} → ${targetT.toFixed(2)}`;
  if(percent)percent.textContent=`${Math.round(editor.previewT*100)}%`;
}

function boundaryV3AddBoundaryAt(fingerprint,perimeter,additive) {
  const editor=boundaryV3State(),draft=editor.draft,local=boundaryV3Screen(draft.localFingerprint,draft),remote=boundaryV3Screen(fingerprint,draft);if(!local||!remote)return;
  const existing=draft.boundaries.filter(boundary=>boundary.peerFingerprint===fingerprint),localTotal=boundaryV3PerimeterMetrics(local).total,targetTotal=boundaryV3PerimeterMetrics(remote).total;
  const before=boundaryClone(draft);
  if(!additive&&existing.length) {
    const boundary=existing[0],mappingBefore=boundaryV3BoundaryMappingKey(boundary),arc=boundaryV3PortalArc(local,boundary.source);boundary.source=boundaryV3ArcPortal(local,perimeter-arc.length/2,arc.length);editor.selectedBoundary=boundary.id;if(mappingBefore===boundaryV3BoundaryMappingKey(boundary)){editor.draft=before;editor.selectedBoundary=boundary.id;render();return;}boundaryV3MarkChanged(boundary);
  } else {
    const serial=draft.boundaries.length+1,id=`manual-${String(Date.now()).slice(-8)}-${serial}`,length=Math.min(localTotal*.2,Math.max(20,localTotal*.08)),targetLength=Math.min(targetTotal*.2,Math.max(20,targetTotal*.08)),targetStart=((perimeter/localTotal+.5)%1)*targetTotal;
    draft.boundaries.push({id,peerFingerprint:fingerprint,mappingRevision:1,localConfirmedRevision:0,peerConfirmedRevision:0,reversed:false,source:boundaryV3ArcPortal(local,perimeter-length/2,length),target:boundaryV3ArcPortal(remote,targetStart-targetLength/2,targetLength)});editor.selectedBoundary=id;
  }
  const overlap=boundaryV3OverlapError(draft);
  if(overlap){editor.draft=before;editor.selectedBoundary=before.boundaries[0]?.id||'';editor.message=overlap;editor.error='';showToast(`操作未完成：${overlap}`);render();return;}
  editor.undo.push(before);if(editor.undo.length>50)editor.undo.shift();
  boundaryV3Save(additive?'已追加独立逻辑边界；原有 boundaryId 保持不变。':'设备边界已移动；对端入口保持原位。');
}

document.addEventListener('click',event=>{
  const editor=boundaryV3State(),view=event.target.closest('[data-v3-view]')?.dataset.v3View;if(view){editor.view=view;render();return;}
  const graphMotion=event.target.closest('[data-v3-graph-motion]')?.dataset.v3GraphMotion;if(graphMotion){boundaryV3SetGraphMotion(graphMotion);return;}
  const manualScreen=event.target.closest('[data-v3-manual-screen-toggle]')?.dataset.v3ManualScreenToggle;if(manualScreen){editor.manualScreen=editor.manualScreen===manualScreen?'':manualScreen;render();return;}
  const select=event.target.closest('[data-v3-select-boundary]')?.dataset.v3SelectBoundary;if(select){editor.selectedBoundary=select;render();return;}
  const action=event.target.closest('[data-v3-action]')?.dataset.v3Action;
  if(action==='toggle-correction'){editor.correctionOpen=!editor.correctionOpen;render();return;}
  const remoteSequence=Number(event.target.closest('[data-v3-accept-remote]')?.dataset.v3AcceptRemote)||0;
  if(remoteSequence){
    editor.message=currentModel().language==='en'?'Saving the peer layout and returning this device’s confirmation…':'正在保存对端布局并回传本机确认…';
    send({action:'accept-boundary-workspace-preview',sequence:remoteSequence});
    render();
    return;
  }
  if(action==='zoom-in'){const viewport=boundaryV3SpatialViewport(editor.draft);boundaryV3SetSpatialZoom(viewport.scale*1.18);return;}
  if(action==='zoom-out'){const viewport=boundaryV3SpatialViewport(editor.draft);boundaryV3SetSpatialZoom(viewport.scale/1.18);return;}
  if(action==='fit'){editor.spatialViewport=boundaryV3FitSpatialViewport(editor.draft);render();return;}
  const screenSelect=event.target.closest('[data-v3-select-screen]')?.dataset.v3SelectScreen;
  if(screenSelect&&!event.target.closest('button,input,summary')){editor.selectedScreen=screenSelect;render();return;}
  const peerSelect=event.target.closest('[data-v3-select-peer-boundary]')?.dataset.v3SelectPeerBoundary;
  if(peerSelect&&!event.target.closest('button')){const first=editor.draft.boundaries.find(boundary=>boundary.peerFingerprint===peerSelect);if(first)editor.selectedBoundary=first.id;render();return;}
  if(editor.saving)return;
  const add=event.target.closest('[data-v3-add-screen]');if(add){boundaryV3AddScreen(add);return;}
  const update=event.target.closest('[data-v3-update-screen]');if(update){boundaryV3UpdateScreen(update);return;}
  const pair=event.target.closest('[data-v3-pair]')?.dataset.v3Pair;if(pair){const boundary=boundaryV3FindBoundary(boundaryV3State().selectedBoundary);if(!boundary)return;if(pair==='confirm-local')boundaryV3ConfirmLocal(boundary);else boundaryV3ShiftTarget(boundary,pair);return;}
  if(!action)return;
  if(action==='generate')boundaryV3GenerateDraft();
  else if(action==='restore-auto')boundaryV3GenerateDraft();
  else if(action==='toggle-auto-reference'){editor.automaticVisible=!editor.automaticVisible;render();}
  else if(action==='undo')boundaryV3Undo();
  else if(action==='delete-boundary')boundaryV3DeleteSelected();
  else if(action==='rotate')boundaryV3RotateScreen(event.target.closest('[data-fingerprint]').dataset.fingerprint);
  else if(action==='remove-screen')boundaryV3RemoveScreen(event.target.closest('[data-fingerprint]').dataset.fingerprint);
  else if(action==='reset-graph'){try{localStorage.removeItem('clipferry.boundaryGraph.v1');}catch(_){}editor.graphStoredPositions={};boundaryV3SaveGraphPositions({});editor.graphPositions=boundaryV3GraphPositions();editor.graphAnchors=Object.fromEntries(Object.entries(editor.graphPositions).map(([id,point])=>[id,{x:point.x,y:point.y}]));editor.graphVelocity={};render();if(editor.graphMotion==='elastic')boundaryV3EnsureGraphAnimation();}
  else if(action==='apply')send({action:'apply-boundary-workspace-v3',expectedStorageRevision:boundaryV3State().storageRevision});
});

document.addEventListener('wheel',event=>{
  if(state.page!=='cross'||!event.target.closest('[data-v3-spatial-svg]'))return;
  event.preventDefault();
  const editor=boundaryV3State(),svg=event.target.closest('[data-v3-spatial-svg]'),viewport=boundaryV3SpatialViewport(editor.draft),factor=event.deltaY<0?1.12:1/1.12;
  boundaryV3SetSpatialZoom(viewport.scale*factor,boundaryV3SvgPoint(svg,event));
},{passive:false});

document.addEventListener('input',event=>{
  if(event.target.matches('[data-v3-preview-t]'))boundaryV3RefreshPairPreview(event.target);
});

document.addEventListener('keydown',event=>{
  if(state.page!=='cross'||hasEditableFocus())return;
  if((event.ctrlKey||event.metaKey)&&event.key.toLowerCase()==='z'){event.preventDefault();boundaryV3Undo();}
  else if(event.key==='Delete'&&boundaryV3State().view==='boundary'){event.preventDefault();boundaryV3DeleteSelected();}
});

document.addEventListener('dragstart',event=>{
  const peer=event.target.closest('[data-v3-peer-drag]');if(!peer)return;
  if(boundaryV3State().saving){event.preventDefault();return;}
  boundaryV3State().peerDrag={fingerprint:peer.dataset.v3PeerDrag,additive:Boolean(event.ctrlKey)};event.dataTransfer.effectAllowed='copyMove';event.dataTransfer.setData('text/plain',peer.dataset.v3PeerDrag);
});
document.addEventListener('dragover',event=>{if(boundaryV3State().peerDrag&&event.target.closest('[data-v3-edit-screen]')){event.preventDefault();event.dataTransfer.dropEffect=event.ctrlKey?'copy':'move';}});
document.addEventListener('drop',event=>{const editor=boundaryV3State(),screenNode=event.target.closest('[data-v3-edit-screen]');if(!editor.peerDrag||!screenNode)return;event.preventDefault();const local=boundaryV3Screen(editor.draft.localFingerprint,editor.draft),perimeter=boundaryV3PointerPerimeter(event,screenNode,local);boundaryV3AddBoundaryAt(editor.peerDrag.fingerprint,perimeter,event.ctrlKey||editor.peerDrag.additive);editor.peerDrag=null;});
document.addEventListener('dragend',()=>{boundaryV3State().peerDrag=null;});

function boundaryV3SvgPoint(svg,event){const point=svg.createSVGPoint();point.x=event.clientX;point.y=event.clientY;const matrix=svg.getScreenCTM();return matrix?point.matrixTransform(matrix.inverse()):point;}

function boundaryV3RefreshPortalDrag(drag,boundary) {
  const stage=document.querySelector('[data-v3-edit-screen]');if(!stage)return;
  const selector=`[data-v3-boundary="${CSS.escape(drag.boundaryId)}"]`;
  const pieces=[...stage.querySelectorAll(`${selector}.boundary-v3-segment`)];
  const segments=boundary[drag.portalName].segments,color=boundaryV3PeerColor(boundary.peerFingerprint);
  segments.forEach((segment,index)=>{
    let piece=pieces[index];
    if(!piece){piece=document.createElement('span');piece.dataset.v3Boundary=drag.boundaryId;piece.dataset.v3Portal=drag.portalName;piece.dataset.v3Piece=String(index);stage.appendChild(piece);}
    piece.className=`boundary-v3-segment ${segment.edge} selected`;
    piece.setAttribute('style',boundaryV3SegmentStyle(segment,color));
  });
  pieces.slice(segments.length).forEach(piece=>{if(piece!==document.activeElement)piece.remove();else piece.style.display='none';});
  const arc=boundaryV3PortalArc(drag.screen,boundary[drag.portalName]);
  const start=stage.querySelector(`${selector}[data-v3-handle="start"]`),end=stage.querySelector(`${selector}[data-v3-handle="end"]`);
  if(start)start.setAttribute('style',`${boundaryV3HandleStyle(drag.screen,arc.start)};--peer:${color}`);
  if(end)end.setAttribute('style',`${boundaryV3HandleStyle(drag.screen,arc.start+arc.length)};--peer:${color}`);
}

document.addEventListener('pointerdown',event=>{
  const editor=boundaryV3State(),graphNode=event.target.closest('[data-v3-graph-node]');
  if(graphNode){event.preventDefault();const svg=graphNode.ownerSVGElement,point=boundaryV3SvgPoint(svg,event);editor.graphDrag={fingerprint:graphNode.dataset.v3GraphNode,pointerId:event.pointerId,svg,last:point};graphNode.setPointerCapture?.(event.pointerId);editor.selectedScreen=graphNode.dataset.v3GraphNode;boundaryV3EnsureGraphAnimation();return;}
  const spatial=event.target.closest('[data-v3-spatial-screen]');
  if(editor.saving&&spatial){event.preventDefault();return;}
  if(spatial){event.preventDefault();const svg=spatial.ownerSVGElement,point=boundaryV3SvgPoint(svg,event),fingerprint=spatial.dataset.v3SpatialScreen,viewport=boundaryV3SpatialViewport(editor.draft);editor.spatialDrag={fingerprint,pointerId:event.pointerId,svg,last:point,viewport,before:boundaryClone(editor.draft)};editor.selectedScreen=fingerprint;spatial.setPointerCapture?.(event.pointerId);return;}
  const panTarget=event.target.closest('[data-v3-spatial-pan]');
  if(panTarget){event.preventDefault();const svg=panTarget.ownerSVGElement,point=boundaryV3SvgPoint(svg,event);editor.spatialPan={pointerId:event.pointerId,svg,start:point,origin:boundaryV3SpatialViewport(editor.draft)};panTarget.setPointerCapture?.(event.pointerId);return;}
  const handle=event.target.closest('[data-v3-handle]'),segment=event.target.closest('[data-v3-boundary]'),screenNode=event.target.closest('[data-v3-edit-screen]');
  if(editor.saving&&(handle||segment)){event.preventDefault();return;}
  if((handle||segment)&&screenNode){event.preventDefault();const boundaryId=(handle||segment).dataset.v3Boundary,boundary=boundaryV3FindBoundary(boundaryId),portalName=(handle||segment).dataset.v3Portal||'source',screen=portalName==='source'?boundaryV3Screen(editor.draft.localFingerprint,editor.draft):boundaryV3Screen(boundary.peerFingerprint,editor.draft),arc=boundaryV3PortalArc(screen,boundary[portalName]),origin=boundaryV3PointerPerimeter(event,screenNode,screen),mode=handle?.dataset.v3Handle||'move',shared=portalName==='source'&&(mode==='start'||mode==='end')?boundaryV3SharedNeighbor(boundary,mode,screen,editor.draft):null;editor.selectedBoundary=boundaryId;editor.portalDrag={boundaryId,portalName,mode,shared,screenNode,screen,origin,arc,before:boundaryClone(editor.draft),pointerId:event.pointerId,startClient:{x:event.clientX,y:event.clientY},moved:false};(handle||segment).setPointerCapture?.(event.pointerId);}
});

window.addEventListener('pointermove',event=>{
  const editor=boundaryV3State();
  if(editor.graphDrag&&event.pointerId===editor.graphDrag.pointerId){event.preventDefault();const drag=editor.graphDrag,point=boundaryV3SvgPoint(drag.svg,event),position=editor.graphPositions[drag.fingerprint],dx=point.x-drag.last.x,dy=point.y-drag.last.y;position.x=clamp(point.x,58,702);position.y=clamp(point.y,58,312);drag.last=point;if(editor.graphMotion==='manual'){editor.graphAnchors[drag.fingerprint]={x:position.x,y:position.y};const local=currentModel().localFingerprint,neighbors=drag.fingerprint===local?(currentModel().peers||[]).map(peer=>peer.fingerprint):[local];neighbors.forEach(id=>{if(!editor.graphVelocity[id])editor.graphVelocity[id]={x:0,y:0};editor.graphVelocity[id].x+=dx*.12;editor.graphVelocity[id].y+=dy*.12;});}boundaryV3RefreshGraphDom();boundaryV3EnsureGraphAnimation();return;}
  if(editor.spatialPan&&event.pointerId===editor.spatialPan.pointerId){event.preventDefault();const drag=editor.spatialPan,point=boundaryV3SvgPoint(drag.svg,event),dx=point.x-drag.start.x,dy=point.y-drag.start.y;editor.spatialViewport={...drag.origin,x:drag.origin.x+dx,y:drag.origin.y+dy};const world=document.querySelector('[data-v3-spatial-world]');if(world)world.setAttribute('transform',`translate(${dx} ${dy})`);return;}
  if(editor.spatialDrag&&event.pointerId===editor.spatialDrag.pointerId){event.preventDefault();const drag=editor.spatialDrag,point=boundaryV3SvgPoint(drag.svg,event),screen=boundaryV3Screen(drag.fingerprint,editor.draft),dx=(point.x-drag.last.x)/drag.viewport.scale,dy=(point.y-drag.last.y)/drag.viewport.scale;if(Math.abs(dx)<.0001&&Math.abs(dy)<.0001)return;screen.canvasXMillimeters=Number(screen.canvasXMillimeters)+dx;screen.canvasYMillimeters=Number(screen.canvasYMillimeters)+dy;drag.last=point;drag.moved=true;editor.automatic=null;boundaryV3RefreshSpatialDrag(drag);boundaryV3SendSpatialPreview();return;}
  if(editor.portalDrag&&event.pointerId===editor.portalDrag.pointerId){event.preventDefault();const drag=editor.portalDrag,boundary=boundaryV3FindBoundary(drag.boundaryId);if(!boundary)return;if(!drag.moved&&Math.hypot(event.clientX-drag.startClient.x,event.clientY-drag.startClient.y)<3)return;drag.moved=true;const current=boundaryV3PointerPerimeter(event,drag.screenNode,drag.screen),total=drag.arc.total;let delta=current-drag.origin;if(delta>total/2)delta-=total;if(delta<-total/2)delta+=total;let start=drag.arc.start,length=drag.arc.length;if(drag.mode==='move')start+=delta;else if(drag.mode==='end')length=clamp(drag.arc.length+delta,.5,total-.5);else{start+=delta;length=clamp(drag.arc.length-delta,.5,total-.5);}boundary[drag.portalName]=boundaryV3ArcPortal(drag.screen,start,length);boundaryV3RefreshPortalDrag(drag,boundary);if(drag.shared){const neighbor=boundaryV3FindBoundary(drag.shared.boundaryId);if(neighbor){const sharedPoint=drag.mode==='start'?((start%total)+total)%total:((start+length)%total+total)%total,neighborStart=drag.mode==='start'?drag.shared.arc.start:sharedPoint,neighborLength=clamp(drag.shared.arc.length+(drag.mode==='start'?delta:-delta),.5,total-.5);neighbor.source=boundaryV3ArcPortal(drag.screen,neighborStart,neighborLength);boundaryV3RefreshPortalDrag({...drag,boundaryId:neighbor.id},neighbor);}}}
},{passive:false});

window.addEventListener('pointerup',event=>{
  const editor=boundaryV3State();
  if(editor.graphDrag&&event.pointerId===editor.graphDrag.pointerId){boundaryV3SaveGraphPositions(editor.graphPositions);editor.graphDrag=null;render();boundaryV3EnsureGraphAnimation();return;}
  if(editor.spatialPan&&event.pointerId===editor.spatialPan.pointerId){editor.spatialPan=null;render();return;}
  if(editor.spatialDrag&&event.pointerId===editor.spatialDrag.pointerId){const drag=editor.spatialDrag,changed=Boolean(drag.moved)&&JSON.stringify(drag.before)!==JSON.stringify(editor.draft);if(changed){editor.draft.screens.forEach(screen=>{screen.canvasXMillimeters=Math.round(Number(screen.canvasXMillimeters));screen.canvasYMillimeters=Math.round(Number(screen.canvasYMillimeters));});editor.undo.push(drag.before);boundaryV3SendSpatialPreview(true);}editor.spatialDrag=null;if(changed)boundaryV3Save('空间位置已保存；自动投影已更新，人工草稿保持不变。');else render();return;}
  if(editor.portalDrag&&event.pointerId===editor.portalDrag.pointerId){const drag=editor.portalDrag,changedIds=[drag.boundaryId,drag.shared?.boundaryId].filter(Boolean).filter(id=>boundaryV3BoundaryMappingKey(drag.before.boundaries.find(boundary=>boundary.id===id))!==boundaryV3BoundaryMappingKey(boundaryV3FindBoundary(id)));editor.portalDrag=null;if(drag.moved&&changedIds.length){const overlap=boundaryV3OverlapError(editor.draft);if(overlap){editor.draft=drag.before;editor.selectedBoundary=drag.boundaryId;editor.saving=false;editor.dirty=false;editor.message=overlap;editor.error='';showToast(`操作未完成：${overlap}`);render();return;}editor.undo.push(drag.before);changedIds.forEach(id=>{const boundary=boundaryV3FindBoundary(id);if(!boundary)return;const arc=boundaryV3PortalArc(drag.screen,boundary.source);if(drag.portalName==='source'&&arc.length<=4)editor.draft.boundaries=editor.draft.boundaries.filter(candidate=>candidate.id!==id);else boundaryV3MarkChanged(boundary);});editor.selectedBoundary=editor.draft.boundaries.some(boundary=>boundary.id===editor.selectedBoundary)?editor.selectedBoundary:(editor.draft.boundaries[0]?.id||'');boundaryV3Save(drag.shared?'共享分割点已调整；压缩到零的逻辑边界会整体移除。':drag.portalName==='source'?'本机出口已调整；对端入口保持原位并等待确认。':'对端入口已调整；本机出口保持原位并等待确认。');}else{editor.draft=drag.before;editor.selectedBoundary=drag.boundaryId;render();}}
});
window.addEventListener('pointercancel',event=>{
  const editor=boundaryV3State();
  if(editor.spatialDrag&&event.pointerId===editor.spatialDrag.pointerId){editor.draft=editor.spatialDrag.before;editor.spatialDrag=null;render();}
  if(editor.portalDrag&&event.pointerId===editor.portalDrag.pointerId){editor.draft=editor.portalDrag.before;editor.portalDrag=null;render();}
  if(editor.graphDrag&&event.pointerId===editor.graphDrag.pointerId){editor.graphDrag=null;render();boundaryV3EnsureGraphAnimation();}
  if(editor.spatialPan&&event.pointerId===editor.spatialPan.pointerId){editor.spatialViewport=editor.spatialPan.origin;editor.spatialPan=null;render();}
});
