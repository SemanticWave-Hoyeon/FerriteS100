<?xml version="1.0" encoding="UTF-8"?>
<Dataset xmlns:gml="http://www.opengis.net/gml/3.2"
    xmlns:s100_profile="http://www.iho.int/S-100/profile/s100_gmlProfile"
    xmlns:xlink="http://www.w3.org/1999/xlink"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
    gml:id="S421.abc.abcde.00002"
    xmlns:S100="http://www.iho.int/s100gml/5.0"
    xmlns:S421="http://www.iec.ch/S421/2.0"
    xsi:schemaLocation="http://www.iec.ch/S421/2.0 ./S-421_schema_2.0.0.xsd"
    xmlns="http://www.iec.ch/S421/2.0"
>
    <gml:boundedBy>
        <gml:Envelope srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
            <gml:lowerCorner>4.4938137 105.9733644</gml:lowerCorner>
            <gml:upperCorner>22.2442299 118.1042359</gml:upperCorner>
        </gml:Envelope>
    </gml:boundedBy>
    <S100:DatasetIdentificationInformation>
        <S100:encodingSpecification>S-100 Part 10b</S100:encodingSpecification>
        <S100:encodingSpecificationEdition>1.0</S100:encodingSpecificationEdition>
        <S100:productIdentifier>S-421</S100:productIdentifier>
        <S100:productEdition>2.0.0</S100:productEdition>
        <S100:applicationProfile>test</S100:applicationProfile>
        <S100:datasetFileIdentifier>S-421 Basic generic test data</S100:datasetFileIdentifier>
        <S100:datasetTitle>Basic Test Route Dataset</S100:datasetTitle>
        <S100:datasetReferenceDate>2026-05-11</S100:datasetReferenceDate>
        <S100:datasetLanguage>eng</S100:datasetLanguage>
        <S100:datasetTopicCategory>transportation</S100:datasetTopicCategory>
        <S100:datasetPurpose>base</S100:datasetPurpose>
        <S100:updateNumber>0</S100:updateNumber>
    </S100:DatasetIdentificationInformation>
    <members>
        <!-- IEC63173-1 12.3.3.1 / 12.3.3.3: valid baseline basic route.
               Use the commented alternatives below to create targeted invalid-import cases.
               This baseline keeps all mandatory objects active, while the waypoint fixed-state
               examples below cover editable, fixed and omitted optional values in one file. -->
        <Route gml:id="RTE">
            <!-- IEC63173-1 12.3.3.2.1 a) Invalid route version format. -->
            <routeFormatVersion>2.0</routeFormatVersion>
            <!-- <routeFormatVersion>2.0.1</routeFormatVersion> -->
            <!-- IEC63173-1 12.3.3.2.1 b) Duplicated route ID.
                 Reuse this routeID in another imported basic route sample. -->
            <routeID>8fc313d7-64d7-4d8e-ae69-886b413d1ee1</routeID>
            <routeEditionNo>1</routeEditionNo>
            <routeInfo xlink:href="#RTE.INFO" xlink:title="routeInfoComposition" />
            <routeSchedules xlink:href="#RTE.SCHEDS" xlink:title="routeSchedulesComposition" />
            <routeWaypoints xlink:href="#RTE.WPTS" xlink:title="routeWaypointsComposition" />
            <routeActionPoints xlink:title="routeActionPointsComposition" xlink:href="#RTE.APTS" />
        </Route>
        <RouteInfo gml:id="RTE.INFO">
            <routeInfoName>BasicGenericTestData</routeInfoName>
            <routeInfoAuthor>Korea VTS</routeInfoAuthor>
            <!-- IEC63173-1 12.3.3.2.1 c) Invalid time value in routeInfoEditionTime. -->
            <!-- <routeInfoEditionTime>2026-06-24T25:00:00Z</routeInfoEditionTime> -->
            <!-- IEC63173-1 12.3.3.2.1 d) Invalid enumeration value in routeInfoStatus. -->
            <routeInfoStatus code="2">Planned</routeInfoStatus>
            <!-- <routeInfoStatus>Confirmed</routeInfoStatus> -->
            <!-- IEC63173-1 12.3.3.2.1 e)-f) Invalid validity start/end times for dedicated negative cases. -->
            <!-- <routeInfoValidityStart>2026-06-31T00:00:00Z</routeInfoValidityStart> -->
            <!-- <routeInfoValidityEnd>2026-06-31T25:00:00Z</routeInfoValidityEnd> -->
            <!-- IEC63173-1 12.3.3.2.1 g) Invalid vessel MMSI format. -->
            <!-- <routeInfoVesselMMSI>44012A456</routeInfoVesselMMSI> -->
            <routeInfoCollection xlink:href="#RTE" xlink:title="routeInfoComposition" />
        </RouteInfo>
        <!-- </RouteInformation> -->
        <RouteSchedules gml:id="RTE.SCHEDS">
            <routeSchedule xlink:title="routeScheduleComposition" xlink:href="#RTE.SCHED.1" />
            <routeSchedulesCollection xlink:href="#RTE" xlink:title="routeSchedulesComposition" />
        </RouteSchedules>
        <RouteSchedule gml:id="RTE.SCHED.1">
            <!-- IEC63173-1 12.2.3 / 12.3.3.2.4:
                 duplicate this schedule block to create a reused routeScheduleID,
                 or retarget one schedule element below to a non-existent waypoint reference.
                 Derived calculated/recommended schedule variants can reuse the same waypoint set. -->
            <routeScheduleID>1</routeScheduleID>
            <routeScheduleCollection xlink:href="#RTE.SCHEDS" xlink:title="routeScheduleComposition" />
            <routeScheduleManual xlink:title="routeScheduleManualComposition" xlink:href="#RTE.SCHED.1.MAN" />
        </RouteSchedule>
        <RouteScheduleManual gml:id="RTE.SCHED.1.MAN">
            <routeScheduleManualCollection xlink:href="#RTE.SCHED.1" xlink:title="routeScheduleManualComposition" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.1" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.2" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.3" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.4" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.5" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.6" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.7" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.8" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.9" />
            <routeScheduleElementManual xlink:title="routeScheduleElementManualComposition" xlink:href="#RTE.SCHED.1.MAN.ELEMENT.10" />
        </RouteScheduleManual>
        <!-- </ManualRouteSchedule> -->
        <!-- IEC63173-1 12.3.3.2.4: edit any manual schedule element below.
             The first two elements include example invalid values for SOG, ETD and ETA. -->
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.1">
            <!-- IEC63173-1 12.2.3 / 12.3.3.2.4:
                 change routeWaypointID or targetWaypoint to an unknown waypoint such as 999 / #RTE.WPT.999
                 when preparing the invalid schedule reference case. -->
            <routeWaypointID>1</routeWaypointID>
            <!-- IEC63173-1 12.3.3.2.4 a) Invalid planned SOG value. -->
            <!-- <routeScheduleElementPlanSOG>-1</routeScheduleElementPlanSOG> -->
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <!-- IEC63173-1 12.3.3.2.4 c) Invalid ETD value. -->
            <!-- <routeScheduleElementETD>2026-06-17T25:00:00Z</routeScheduleElementETD> -->
            <routeScheduleElementETD>2026-06-17T15:00:00Z</routeScheduleElementETD>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.1" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.2">
            <routeWaypointID>2</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-18T00:00:00Z</routeScheduleElementETD>
            <!-- IEC63173-1 12.3.3.2.4 b) Invalid ETA value. -->
            <!-- <routeScheduleElementETA>2026-06-18T99:00:00Z</routeScheduleElementETA> -->
            <routeScheduleElementETA>2026-06-18T00:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.2" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.3">
            <routeWaypointID>3</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-18T09:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-18T09:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.3" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.4">
            <routeWaypointID>4</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-18T18:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-18T18:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.4" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.5">
            <routeWaypointID>5</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-19T02:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-19T02:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.5" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.6">
            <routeWaypointID>6</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-19T11:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-19T11:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.6" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.7">
            <routeWaypointID>7</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-19T20:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-19T20:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.7" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.8">
            <routeWaypointID>8</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-20T05:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-20T05:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.8" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.9">
            <routeWaypointID>9</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETD>2026-06-20T14:00:00Z</routeScheduleElementETD>
            <routeScheduleElementETA>2026-06-20T14:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.9" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteScheduleElement gml:id="RTE.SCHED.1.MAN.ELEMENT.10">
            <routeWaypointID>10</routeWaypointID>
            <routeScheduleElementPlanSOG>15.0</routeScheduleElementPlanSOG>
            <routeScheduleElementETA>2026-06-20T23:00:00Z</routeScheduleElementETA>
            <routeScheduleElementManualCollection xlink:href="#RTE.SCHED.1.MAN" xlink:title="routeScheduleElementManualComposition" />
            <targetWaypoint xlink:href="#RTE.WPT.10" xlink:title="routeWaypointSchedule" />
        </RouteScheduleElement>
        <RouteWaypoints gml:id="RTE.WPTS">
            <!-- IEC63173-1 12.3.3.1 h)-i) / 12.3.3.3.4 b)-c):
                 this collection is ordered and contains 10 unique waypoint references.
                 Reorder or delete references and matching waypoint objects below for the relevant tests. -->
            <routeWaypointsCollection xlink:title="routeWaypointsComposition" xlink:href="#RTE" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.1" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.2" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.3" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.4" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.5" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.6" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.7" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.8" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.9" />
            <routeWaypoint xlink:title="routeWaypointComposition" xlink:href="#RTE.WPT.10" />
        </RouteWaypoints>
        <RouteWaypoint gml:id="RTE.WPT.1">
            <!-- IEC63173-1 12.3.3.2.3 a) Duplicated waypoint id.
                 Clone a waypoint or change another routeWaypointID to match the value below. -->
            <routeWaypointID>1</routeWaypointID>
            <routeWaypointName>WP1</routeWaypointName>
              <!-- IEC63173-1 12.3.3.3.4 d) Editable baseline example. Change this to true when preparing
                  a dedicated fixed-waypoint edit test without using the active fixed example in WPT.2. -->
            <routeWaypointFixed>false</routeWaypointFixed>
            <!-- IEC63173-1 12.3.3.1 g) / 12.3.3.2.3 c)-d):
                 use the commented values below for out-of-range or invalid-resolution turn radius tests. -->
            <routeWaypointTurnRadius>2</routeWaypointTurnRadius>
            <!-- <routeWaypointTurnRadius>5.1</routeWaypointTurnRadius> -->
            <!-- <routeWaypointTurnRadius>2.05</routeWaypointTurnRadius> -->
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.1" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <!-- IEC63173-1 12.3.3.1 e)-f) / 12.3.3.2.3 b):
                 keep this as a GM_Point with at least 0.0001 degree resolution for the valid baseline.
                 For the geometry-type negative case, replace S100:Point with a non-point geometry in this block. -->
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.1.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>22.2442299 118.1042359</gml:pos>
                        <!-- <gml:pos>22.244 118.104</gml:pos> -->
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.2">
            <routeWaypointID>2</routeWaypointID>
            <routeWaypointName>WP2</routeWaypointName>
            <!-- IEC63173-1 12.2.3: active fixed waypoint example for mixed fixed/editable coverage. -->
            <routeWaypointFixed>true</routeWaypointFixed>
            <routeWaypointTurnRadius>2</routeWaypointTurnRadius>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.2" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.1" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.2.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>20.7860128 116.4340427</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.3">
            <routeWaypointID>3</routeWaypointID>
            <routeWaypointName>WP3</routeWaypointName>
            <!-- IEC63173-1 12.2.3: keep routeWaypointFixed omitted here to cover the no-value case. -->
            <routeWaypointTurnRadius>2</routeWaypointTurnRadius>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.3" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.2" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.3.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>19.1060479 115.0568668</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.4">
            <routeWaypointID>4</routeWaypointID>
            <routeWaypointName>WP4</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2</routeWaypointTurnRadius>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.4" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.3" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.4.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>17.1570596 114.1192153</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.5">
            <routeWaypointID>5</routeWaypointID>
            <routeWaypointName>WP5</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2</routeWaypointTurnRadius>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.5" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.4" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.5.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>14.9327529 113.2108638</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.6">
            <routeWaypointID>6</routeWaypointID>
            <routeWaypointName>WP6</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2</routeWaypointTurnRadius>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.6" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.5" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.6.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>12.1415098 112.9178492</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.7">
            <routeWaypointID>7</routeWaypointID>
            <routeWaypointName>WP7</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2</routeWaypointTurnRadius>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.7" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.6" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.7.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>9.6963651 112.6541345</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.8">
            <routeWaypointID>8</routeWaypointID>
            <routeWaypointName>WP8</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2</routeWaypointTurnRadius>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.8" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.7" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.8.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>7.6109595 110.8081315</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.9">
            <routeWaypointID>9</routeWaypointID>
            <routeWaypointName>WP9</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2</routeWaypointTurnRadius>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.9" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.8" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.9.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>6.069203 108.6691139</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <RouteWaypoint gml:id="RTE.WPT.10">
            <routeWaypointID>10</routeWaypointID>
            <routeWaypointName>WP10</routeWaypointName>
            <routeWaypointFixed>false</routeWaypointFixed>
            <routeWaypointTurnRadius>2</routeWaypointTurnRadius>
            <scheduleOf xlink:href="#RTE.SCHED.1.MAN.ELEMENT.10" xlink:title="routeWaypointSchedule" />
            <routeWaypointCollection xlink:href="#RTE.WPTS" xlink:title="routeWaypointComposition" />
            <routeWaypointLeg xlink:href="#RTE.WPT.LEG.9" xlink:title="routeWaypointLegComposition" />
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.WPT.10.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>4.4938137 105.9733644</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteWaypoint>
        <!-- IEC63173-1 12.3.3.2.3 e)-f):
             use the first leg below for invalid geometry-type tests,
             or edit two adjacent waypoint positions to create a 180 degree meridional difference. -->
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.1">
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <!-- <routeWaypointLegGeometryType>invalidGeometryType</routeWaypointLegGeometryType> -->
            <routeWaypointLegCollection xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.2" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.2">
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegCollection xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.3" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.3">
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegCollection xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.4" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.4">
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegCollection xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.5" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.5">
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegCollection xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.6" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.6">
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegCollection xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.7" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.7">
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegCollection xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.8" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.8">
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegCollection xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.9" />
        </RouteWaypointLeg>
        <RouteWaypointLeg gml:id="RTE.WPT.LEG.9">
            <routeWaypointLegGeometryType>loxodrome</routeWaypointLegGeometryType>
            <routeWaypointLegCollection xlink:title="routeWaypointLegComposition" xlink:href="#RTE.WPT.10" />
        </RouteWaypointLeg>
        <!-- IEC63173-1 12.3.3.2.2 / 12.3.3.3.3:
             clone the action point below when preparing duplicate-id or multi-action-point cases. -->
        <RouteActionPoints gml:id="RTE.APTS">
            <routeActionPointsCollection xlink:href="#RTE" xlink:title="routeActionPointsComposition" />
            <routeActionPoint xlink:title="routeActionPointComposition" xlink:href="#RTE.APT.1" />
        </RouteActionPoints>
        <RouteActionPoint gml:id="RTE.APT.1">
            <!-- IEC63173-1 12.3.3.2.2 a) Duplicate this action point and keep the same identifier strategy
                 when preparing a duplicated action point id test. -->
            <routeActionPointID>1</routeActionPointID>
            <routeActionPointDistance>0.5</routeActionPointDistance>
            <!-- IEC63173-1 12.3.3.2.2 e) Invalid action point required action enumeration value. -->
            <routeActionPointRequiredAction code="2">Report</routeActionPointRequiredAction>
            <!-- <routeActionPointRequiredAction>Recording</routeActionPointRequiredAction> -->
            <routeActionPointRequiredActionDescription>Report to VTS</routeActionPointRequiredActionDescription>
            <!-- IEC63173-1 12.3.3.3.3 a) Include timeToAct for positive export verification when needed. -->
            <!-- <routeActionPointTimeToAct>5</routeActionPointTimeToAct> -->
            <routeActionPointCollection xlink:href="#RTE.APTS" xlink:title="routeActionPointComposition" />
            <!-- IEC63173-1 12.3.3.2.2 b)-d):
                 keep this as a GM_Point for the valid baseline.
                 Use an invalid coordinate/resolution in gml:pos for geometry tests.
                 The radius-misuse case requires extending this point with the optional radius field. -->
            <geometry>
                <S100:pointProperty>
                    <S100:Point gml:id="RTE.APT.1.PT" srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
                        <gml:pos>10.9409139 112.8873593</gml:pos>
                    </S100:Point>
                </S100:pointProperty>
            </geometry>
        </RouteActionPoint>
    </members>
</Dataset>